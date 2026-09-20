//! Row retirement, keyed by the reverse join through `node.sessions[]`
//!.
//!
//! A worker's registry row leaves when its WORK is done and its transcript
//! is quiet. WORK-done is the graph's `status == "done"` read through the
//! reverse join ([`crate::graph_store::work_state`]) over every node the
//! session is named on; quiet is the served transcript mtime past the retire
//! grace. Nothing waits for a session to end, because a session never ends
//! (d-10a72d88): the exit-stamp machinery this module used to carry
//! (`exited_at`, `StampExit`, corroboration gates, the backstop, the dormant
//! probe) asked a question with no answer and is deleted.
//!
//! The row question and the worktree question are DIFFERENT questions with
//! different keys. A row retires on work-done plus quiet regardless of its
//! worktree; the tree is then governed by its own bucket (dirty never,
//! clean-and-unmerged never, clean-and-merged loses the tree and keeps the
//! branch), so a dirty tree never pins a finished row and a row retirement
//! never destroys an unmerged branch's only checkout.
//!
//! This module holds the pure decision plus the sweep shells. The sweep body
//! lives in `gc_sweep.rs`. All I/O - the graph read, the transcript stat, the
//! stop, the worktree probes, the clock - is injected, so the policy is
//! unit-testable in isolation.

use crate::graph_store::WorkState;
use std::path::Path;
use std::time::Duration;

/// The row verdict: retire now, or keep with a named reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcAction {
    /// The row leaves the registry: work done on every named node, transcript
    /// quiet past the grace. The receipt records the resumable handle first.
    Retire,
    /// The row stays, and [`KeepReason`] names the gate holding it.
    Keep,
}

/// The probed facts about one registry row the retirement policy needs.
#[derive(Debug, Clone)]
pub struct GcRow {
    /// The row's `origin` (state.rs): only `"operator"` protects, and only
    /// `"spawn"` retires; `None` is not the same fact as either.
    pub origin: Option<String>,
    /// An orchestrator crown rides this row (US9). A crowned row is never
    /// retired by a sweep.
    pub crowned: bool,
    /// WORK-done through the reverse join: named on which nodes, and are they
    /// all `done`.
    pub work: WorkState,
    /// Seconds since the row's transcript was last written, from the served
    /// harness store. `None` = unresolved, and an unresolved transcript is
    /// NEVER quiet.
    pub transcript_age_s: Option<i64>,
    /// Does this row own a REMOVABLE worktree? False for a one-shot ask, a
    /// row in the canonical checkout, or a cwd that is not a linked worktree.
    pub owns_worktree: bool,
    /// Worktree cleanliness: `Some(true)` clean, `Some(false)` dirty,
    /// `None` the probe could not answer (fail closed -> tree kept).
    pub worktree_clean: Option<bool>,
    /// Is the worktree's branch merged into the main line? `Some(true)`,
    /// `Some(false)`, `None` (nothing names the work or the main line).
    /// Asked only after cleanliness answered `true`.
    pub branch_merged: Option<bool>,
    /// The `(node, status)` pairs of EVERY node this session is named on,
    /// when the row is a PLANNING assignment (blueprint/think phase, or a
    /// `bp-` dispatch label): a planner's job ends at plan-written-and-node-
    /// ready, never at feature-shipped. `None` for every other row - their
    /// open-work gate is unchanged.
    pub planning: Option<Vec<(String, String)>>,
    /// The node ids THIS session actually closed on the planning lane: its
    /// own blueprint/think sessions[] row carrying a non-empty `ended_at`
    /// (x-dddd task 1.2). Empty on any other row. An empty set never
    /// retires a planner: an assignment nobody closed stays outstanding.
    pub planning_closed: Vec<String>,
    /// Marker 2 (d-81c6da7e): the node ids where THIS session wrote the
    /// node's plan - the node's `plan_path` names an existing file and no
    /// other session's planning row on it started earlier. The sweep
    /// resolves it from the graph; empty on every row that did not plan.
    pub planning_plan_written: Vec<String>,
    /// A `reap --release` ruling matched this row's planning hold: the
    /// marker question is answered (the operator ruled the assignment
    /// over), so only the 1200 s quiet gate remains.
    pub planning_released: bool,
    /// The row's latest inside-leg report reads `done`: the turn
    /// ended and the session is not waiting. A halted planner - one whose
    /// turn ended with no plan on the node - counts as finished, never as
    /// an assignment in flight. `blocked` (waiting on input) and `working`
    /// keep the planner hold.
    pub turn_ended: bool,
    /// A hold computed beside the work verdict: the cascade's
    /// conflict between witnesses, or the PR-state confirm contradicting a
    /// done node. Decided in the sweep where the route and the graph read
    /// live; relayed here so the keep is named by the policy, never silently
    /// dropped. `None` when nothing holds.
    pub confirm_hold: Option<KeepReason>,
    /// The harness-published terminal state for this session (`done`,
    /// `stopped`, `failed`), when the harness publishes one (change
    /// 1). `None` covers both "not terminal" and "no such instrument":
    /// neither is evidence.
    pub session_terminal: Option<String>,
    /// A LIVE newer registry row resolves the same node this row does
    /// (change 3), formatted `{name} (created {ts})`. The node's
    /// openness justifies that row, not this one.
    pub superseded_by_live_peer: Option<String>,
    /// The open node's RECORDED `merge_status` reads `merged` (
    /// change 6): the status field can lag the merge by minutes when
    /// reconcile is slow. Recorded evidence outranks the lagging status.
    pub node_merged: bool,
    /// The row's own pid answered ESRCH (change 8): a provably dead
    /// process. Death overrides transcript recency - a dead process writes
    /// nothing, so a fresh mtime without a living writer is an artifact -
    /// but an absent or unanswerable pid never does: only ESRCH is death.
    pub pid_gone: bool,
    /// A `reap --release` ruling for THIS row: the release lifts
    /// the transcript-unresolved gate, so an absent transcript age retires
    /// instead of holding. Set only when the verb's ruling matched the row;
    /// a missing age is never quiet on any other path.
    pub release_quiet: bool,
    /// The `(node, pr)` THIS session drives and the PR still reads open
    /// (or was never asked): the session has a `do` row on the open node,
    /// the node carries `pr_number`, its recorded `merge_status` is not
    /// `merged`, and the PR-state read did not answer merged or closed.
    pub open_pr: Option<(String, u64)>,
    /// The live newer peer on the same node has its own `do` row on the
    /// node: the peer drives the PR, so this row's open-PR keep does not
    /// apply and the ordinary release path answers.
    pub peer_drives_pr: bool,
    /// GitHub answered that this session's PR is merged or closed: the
    /// session has nothing left to drive, so the row releases like any
    /// other finished work and falls to the grace gate.
    pub pr_settled: bool,
    /// An adopted row that is provably a registry corpse: a claude row
    /// absent from a KNOWN roster snapshot, or a recorded pid that answered
    /// ESRCH. The origin gate skips such a row, so it is judged like any
    /// other row; every downstream gate still applies.
    pub origin_corpse: bool,
    /// The open-work window (change 2): how long an OPEN-work row may
    /// sit transcript-quiet before its node stops counting as evidence of a
    /// live session. Quiet past it, the row falls to the same grace gate a
    /// done row takes; inside it, the keep names the pinning node. Resolved
    /// from `agents.reap.open_work_retire_s`, defaulting well above the
    /// ordinary grace.
    pub open_work_retire_s: i64,
}

impl GcRow {
    /// The session-shaped release: the ONE predicate the policy
    /// arm and the sweep's obligation yields both read, so they cannot
    /// drift. A released row's own open do row is the stale record of work
    /// that moved on, never a live assignment.
    pub fn session_released(&self) -> bool {
        self.session_terminal.is_some()
            || self.superseded_by_live_peer.is_some()
            || matches!(&self.work, WorkState::Open { status, .. }
                if INACTIVE_NODE_STATUSES.contains(&status.as_str()))
            || self.node_merged
            || self.pr_settled
    }
}

/// The statuses that complete a PLANNING assignment: the plan was written
/// and the node moved on (dispatched, in flight, or shipped). `idea` is the
/// loud exception - an idea node never received the plan, so the planning
/// assignment it was meant for is not finished (AC3-EDGE: an uncompleted
/// revision assignment stays outstanding).
pub const PLANNING_COMPLETE_STATUSES: [&str; 5] =
    ["done", "ready", "in_progress", "in_review", "shipped"];

/// Statuses that finish a planning assignment with NO marker: a
/// node that moved to `deferred` or `superseded` has nothing left to plan.
/// The assignment is over even though this session wrote no close and no
/// plan - waiting forever on a moved-on node is the hold it cures.
pub const PLANNING_MOVED_ON_STATUSES: [&str; 2] = ["deferred", "superseded"];

/// The idle grace for a PLANNER row whose planning assignment is finished
/// (law d-81c6da7e): 20 quiet minutes, not the 900 s every other row takes.
/// A ruled constant, not a config key: the law fixes the number.
pub const PLANNING_IDLE_RETIRE_SECS: i64 = 1200;

/// Node statuses that are NOT active work. A parked or never-started node
/// is not evidence that a session is alive, so it does not shield one
/// (change 6). `superseded` is deliberately absent: a superseded
/// node's work moved elsewhere and the row's own supersession is a registry
/// question, not a node-status one.
pub const INACTIVE_NODE_STATUSES: [&str; 2] = ["deferred", "idea"];

/// WHICH gate is holding a [`GcAction::Keep`] row. Every keep is named - a
/// row that is stuck and invisible is the failure mode this enum exists to
/// prevent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeepReason {
    /// `origin: operator`: a human's row, never touched by a sweep.
    Operator,
    /// A crowned orchestrator row.
    Crowned,
    /// Origin is not `spawn` (adopted, or nothing recorded): only a row fno
    /// itself spawned retires, whatever the work state says.
    NotSpawn { origin: String },
    /// No declared source resolved a node for the session: the
    /// reverse join, the registry field, the row name, and the transcript
    /// all answered nothing (d-bbcd48b5 recovers provenance from any
    /// declared source; nothing left to recover from is the one honest
    /// keep).
    NoProvenance,
    /// Two provenance sources resolved DIFFERENT nodes: witnesses
    /// that disagree are not evidence, so the row is held rather than
    /// retired on a guess.
    NodeConflict { a: String, b: String },
    /// The node reads done but its PR state contradicts: an open
    /// additional PR, or a RECORDED merge_status that is not `merged`. An
    /// absent merge_status does not hold - absence has three explanations
    /// and none is `unmerged` - and rides the basis as unrecorded instead.
    PrStateContradicts { node: String, detail: String },
    /// The node reads planning-complete, but THIS session holds neither
    /// finished marker (x-dddd, d-81c6da7e): its own blueprint/think row
    /// carries no `ended_at`, and it did not write the node's plan. The
    /// completion belongs to an earlier assignment, so this quiet
    /// replanning worker keeps its row with the node and its status named.
    PlanningUnclosed { node: String, status: String },
    /// At least one named node is not done; the first open one is reported.
    OpenWork { node: String, status: String },
    /// Open work whose transcript is quiet INSIDE the open-work window
    /// (change 2): the row keeps for now, but the keep has a clock -
    /// quiet past the window falls to the grace gate - and it names the
    /// stale node pinning it, so an operator can act on the node.
    OpenWorkStale { node: String, status: String },
    /// The transcript was written inside the grace window: the session is
    /// live in the only sense the law allows. A terminal harness state
    /// overrides it (the roster's `done` is not a turn boundary), and so
    /// does a dead pid.
    Active { age_s: i64 },
    /// The transcript could not be resolved. Absence is not quiet.
    TranscriptUnresolved,
    /// The graph could not be read this sweep. Never a retirement on a
    /// failed read.
    GraphUnreadable,
    /// Every named node is done but one still carries an OPEN do row for
    /// this session: settled work would be re-opened by the retirement's
    /// absence, so the row stays and the node is named.
    OpenDoRow { node: String },
    /// The session's node carries an open PR (`pr_number` set, recorded
    /// `merge_status` not `merged`): a retirement here strands the PR with
    /// nothing left to drive it. The remedy is merge, not reap.
    OpenPr { node: String, pr: u64 },
}

impl KeepReason {
    /// Stable, human-readable tag for CLI/JSON output.
    pub fn as_str(&self) -> &'static str {
        match self {
            KeepReason::Operator => "operator",
            KeepReason::Crowned => "crowned",
            KeepReason::NotSpawn { .. } => "not a spawn row",
            KeepReason::NoProvenance => {
                "no provenance: no source resolved a node (sessions, registry, name, transcript)"
            }
            KeepReason::NodeConflict { .. } => "sources disagree",
            KeepReason::PrStateContradicts { .. } => "pr state contradicts",
            KeepReason::PlanningUnclosed { .. } => {
                "planning assignment not finished by this session"
            }
            KeepReason::OpenWork { .. } => "open work",
            KeepReason::OpenWorkStale { .. } => {
                "open work inside the retire window: the stale node pins the row"
            }
            KeepReason::Active { .. } => "active",
            KeepReason::TranscriptUnresolved => "transcript unresolved",
            KeepReason::GraphUnreadable => "graph unreadable",
            KeepReason::OpenDoRow { .. } => "open do row on done node",
            KeepReason::OpenPr { .. } => "open pr",
        }
    }
}

/// The tree verdict for a RETIRED row's worktree. Asked only after the row
/// verdict is `Retire`; a keep-shaped tree never blocks the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeAction {
    /// Remove the tree, keep the branch: clean and merged.
    Prune,
    /// Dirty: uncommitted or untracked content. Tree kept, named.
    KeepDirty,
    /// Clean but the branch never merged: abandoned-but-real work a human
    /// judges. Tree kept, named.
    KeepUnmerged,
    /// The cleanliness probe could not answer. Tree kept, named.
    KeepUnprobed,
    /// The row owns no removable worktree.
    None,
}

/// The one row decision. Order matters and each gate names itself: operator,
/// crown, provenance, open work, transcript, grace, retire. No boolean folds
/// two questions together.
pub fn gc_decide(row: &GcRow, grace_secs: i64) -> (GcAction, Option<KeepReason>) {
    if row.origin.as_deref() == Some("operator") {
        return (GcAction::Keep, Some(KeepReason::Operator));
    }
    if row.crowned {
        return (GcAction::Keep, Some(KeepReason::Crowned));
    }
    // Only a row fno itself spawned retires. An `adopted` row (a session the
    // operator took over) or a row with no origin recorded is someone else's
    // fact about a session, and done-plus-quiet does not make it fno's to
    // remove - unless the row is provably a registry corpse, in which case
    // there is no session left to own it and the row is judged like any
    // other: every downstream gate still applies.
    if row.origin.as_deref() != Some("spawn") && !row.origin_corpse {
        let origin = row.origin.clone().unwrap_or_default();
        return (GcAction::Keep, Some(KeepReason::NotSpawn { origin }));
    }
    // The cascade's holds relay through here so every keep is named by the
    // policy: a conflict between witnesses, or PR evidence contradicting a
    // done node. Gate order is unchanged - operator, crown and origin
    // outrank it, exactly as they outrank the provenance arm below.
    if let Some(hold) = &row.confirm_hold {
        return (GcAction::Keep, Some(hold.clone()));
    }
    match &row.work {
        WorkState::NoProvenance => {
            // The row's own finished report is a positive marker:
            // `turn_ended` says the latest inside-leg report reads done and
            // fno never stopped the row. The grace gate supplies the quiet
            // conjunct, so a done-and-quiet row with no node releases, and
            // a row that never reported done keeps exactly as before.
            if row.turn_ended || row.session_released() {
                grace_gate(row, grace_secs)
            } else {
                (GcAction::Keep, Some(KeepReason::NoProvenance))
            }
        }
        WorkState::Open { node, status } => {
            // The planning lane: the ROW's own job (write the plan) ends at
            // node-ready, so a planner whose every named node has moved past
            // planning is done with its work even though the feature is not
            // shipped (AC3-HP). One open node still parked at `idea` (or any
            // non-complete status) holds the row: the plan it was dispatched
            // to write never landed there.
            // x-dddd task 1.2 binds that verdict to the CURRENT assignment:
            // every node must ALSO carry a finished marker - either this
            // session's own blueprint/think row on it carries `ended_at`,
            // or (d-81c6da7e) this session wrote the node's plan. A quiet
            // replanning worker inherits no completion an earlier blueprint
            // wrote.
            // d-81c6da7e: a finished planner's quiet gate is 1200 s, not
            // the 900 s every other row takes. A released planner skipped
            // the marker question by ruling, so only its quiet gate is
            // left.: the lane keeps precedence over the
            // session-shaped releases below, and an unfinished assignment
            // holds even when the node's status would free a non-planner.
            if let Some(assignments) = &row.planning {
                if row.planning_released {
                    return grace_gate(row, PLANNING_IDLE_RETIRE_SECS);
                }
                // An EMPTY status set fails closed: a lane that fires on a
                // vacuous all() would retire a row the graph could not
                // describe. It falls through to the open-work gate below.
                if !assignments.is_empty() {
                    // three facts finish an assignment beside the
                    // markers - the node moved on (deferred or superseded,
                    // nothing left to plan), and the planner halted (its
                    // last inside-leg report reads done: the turn ended
                    // with no plan, and it is not waiting on anything).
                    let unfinished = assignments.iter().find(|(n, s)| {
                        let moved_on = PLANNING_MOVED_ON_STATUSES.contains(&s.as_str());
                        let marked = row.planning_closed.contains(n)
                            || row.planning_plan_written.contains(n);
                        let complete = PLANNING_COMPLETE_STATUSES.contains(&s.as_str()) && marked;
                        !(moved_on || complete || row.turn_ended)
                    });
                    return match unfinished {
                        None => grace_gate(row, PLANNING_IDLE_RETIRE_SECS),
                        Some((n, s)) => (
                            GcAction::Keep,
                            Some(KeepReason::PlanningUnclosed {
                                node: n.clone(),
                                status: s.clone(),
                            }),
                        ),
                    };
                }
            }
            // The open-PR keep outranks every session-shaped release below:
            // a terminal roster state, a parked node, or a live peer that
            // does not drive the PR would strand a real PR with nothing
            // left to drive it. The recorded-merge release cannot meet
            // this arm, because merge_status merged means no open PR.
            if let Some((node, pr)) = &row.open_pr {
                if !row.peer_drives_pr {
                    return (
                        GcAction::Keep,
                        Some(KeepReason::OpenPr {
                            node: node.clone(),
                            pr: *pr,
                        }),
                    );
                }
            }
            // changes 1, 3, 6, 8: open NODE state alone is not
            // evidence a SESSION is alive. Four positive facts say this
            // row's own story is over, and each falls through to the same
            // grace gate a done node takes (the transcript gates keep this
            // from being a blanket sweep):
            // - the harness publishes a terminal state for the session;
            // - a live newer registry row resolves the same node;
            // - the node is parked (deferred) or never started (idea);
            // - the node's recorded merge_status already reads merged.
            // Dead pid is change 8 and rides the grace gate itself.
            if row.session_released() {
                return grace_gate(row, grace_secs);
            }
            // change 2: open NODE state alone is not evidence a
            // SESSION is alive, and inside this window neither is an open
            // node plus quiet. A row quiet past the open-work window falls
            // to the same grace gate a released row takes; a row inside it
            // keeps, and the keep names the node pinning it, so an operator
            // can act on the node rather than on the row. An UNRESOLVED
            // transcript has no clock to age past anything, so it keeps
            // under the unchanged open-work reason.
            match row.transcript_age_s {
                Some(age) if age > row.open_work_retire_s => grace_gate(row, grace_secs),
                Some(_) => (
                    GcAction::Keep,
                    Some(KeepReason::OpenWorkStale {
                        node: node.clone(),
                        status: status.clone(),
                    }),
                ),
                None => (
                    GcAction::Keep,
                    Some(KeepReason::OpenWork {
                        node: node.clone(),
                        status: status.clone(),
                    }),
                ),
            }
        }
        WorkState::AllDone { .. } => grace_gate(row, grace_secs),
    }
}

/// The transcript gates shared by every retire-eligible arm: an unresolved
/// transcript and a transcript inside the grace window both keep the row.
fn grace_gate(row: &GcRow, grace_secs: i64) -> (GcAction, Option<KeepReason>) {
    match row.transcript_age_s {
        // the release lifts this one gate for this one row. Every
        // other path reads absence as unresolved, never as quiet.
        None if row.release_quiet => (GcAction::Retire, None),
        None => (GcAction::Keep, Some(KeepReason::TranscriptUnresolved)),
        // change 8: a provably dead pid (ESRCH) overrides recency.
        // Recency without a living writer is not liveness; only ESRCH
        // revokes it, never an absent or unanswerable pid.
        // a terminal harness state overrides recency too. The
        // live roster shows a between-turns session as working/idle, never
        // done - `done` is not a turn boundary, it is the finish line.
        Some(age) if age <= grace_secs && !row.pid_gone && row.session_terminal.is_none() => {
            (GcAction::Keep, Some(KeepReason::Active { age_s: age }))
        }
        Some(_) => (GcAction::Retire, None),
    }
}

/// The tree verdict for a row the policy just retired. Runs ONLY on Retire:
/// row retirement makes a tree eligible and never bypasses the bucket.
pub fn tree_action(row: &GcRow) -> TreeAction {
    if !row.owns_worktree {
        return TreeAction::None;
    }
    match row.worktree_clean {
        None => TreeAction::KeepUnprobed,
        Some(false) => TreeAction::KeepDirty,
        Some(true) => match row.branch_merged {
            Some(true) => TreeAction::Prune,
            _ => TreeAction::KeepUnmerged,
        },
    }
}

/// The one handle a row is both PROBED and REPORTED under. `fno agents truth`
/// resolves a row by short_id or by name, so the fallback is a real handle,
/// not a display string. Written once so the sweep never probes under one
/// name and reports under another.
pub fn row_handle(e: &crate::state::RegistryEntry) -> String {
    // The session id resolves for BOTH populations (`fno agents truth`
    // answers a full harness session id for registry and roster rows alike);
    // an fno short id resolves for registry rows only, so a roster-only
    // row probed under its short id always answered not-found and the
    // sweep built for it could never age one.
    if let Some(sid) = e.harness_session_id.as_deref().filter(|s| !s.is_empty()) {
        return sid.to_string();
    }
    if e.short_id.is_empty() {
        e.name.clone()
    } else {
        e.short_id.clone()
    }
}

/// The label a sweep REPORTS a row under: the short id, falling back to the
/// name - the operator-facing identity every summary bucket, hold, and
/// release ruling keys on. change 1 split this from [`row_handle`],
/// the PROBE handle: the probe must ask the harness session id, while the
/// report keeps the handle an operator (and `reap --release`) already holds.
pub fn row_label(e: &crate::state::RegistryEntry) -> String {
    if e.short_id.is_empty() {
        e.name.clone()
    } else {
        e.short_id.clone()
    }
}

/// The production transcript-age seam: the NEWEST TIMESTAMPED
/// transcript entry, read through the shared truth probe (one batched,
/// single-flighted child per sweep) - not a file stat, whose untimestamped
/// trailing records keep a dead file reading fresh (measured median +20 min,
/// max +240 h). A handle the probe cannot resolve is absent from the map, and
/// the sweep reads absence as `None`: an unresolved transcript is never a
/// quiet one.
pub fn probe_entry_ages(
    entries: &[&crate::state::RegistryEntry],
) -> std::collections::HashMap<String, Option<i64>> {
    let handles: Vec<String> = entries.iter().map(|e| row_handle(e)).collect();
    if handles.is_empty() {
        return std::collections::HashMap::new();
    }
    crate::truth_probe::family1_truth_probe_many(&handles)
        .into_iter()
        .map(|(handle, probe)| (handle, probe.last_activity_age_s.map(|a| a as i64)))
        .collect()
}

// --- the sweep shells -------------------------------------------------------
// The two triggers behind the pure decision above: the daemon's idle tick and
// the manual `fno agents reap` verb both shell these. The sweep body lives in
// `gc_sweep.rs`; these resolve the production seams (graph read, store index,
// stop lane) and call it.

use crate::events::EventEmitter;
use crate::gc_sweep;
use crate::paths::AgentsHome;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// The next guard window's interval, resolved off-loop by the sweep body and
/// handed back for later ticks to read: the idle-probe verdict pattern, so
/// the select arm never blocks on config reads. Empty until the first sweep
/// lands, which reads as the default.
pub type RetireIntervalCell = Mutex<Option<Duration>>;

/// The interval the next guard window compares against: the last value the
/// sweep body handed back, or [`crate::agents_config::DEFAULT_RETIRE_INTERVAL_SECS`]
/// before the first handoff.
pub fn retire_interval_snapshot(cell: &Arc<RetireIntervalCell>) -> Duration {
    Duration::from_secs(
        cell.lock()
            .ok()
            .and_then(|c| *c)
            .map(|d| d.as_secs())
            .unwrap_or(crate::agents_config::DEFAULT_RETIRE_INTERVAL_SECS),
    )
}

/// Boot-time seed for the interval cell: one config read at daemon start so
/// the first guard window already honors the configured cadence instead of
/// waiting out the default; the sweep body keeps the cell fresh off-loop
/// afterward.
pub fn seed_retire_interval_cell(grace_cwd: &std::path::Path) -> Arc<RetireIntervalCell> {
    let grace = crate::agents_config::retire_grace_secs(grace_cwd);
    Arc::new(Mutex::new(Some(Duration::from_secs(
        crate::agents_config::retire_interval_s(grace_cwd, grace),
    ))))
}

/// The daemon idle tick's retirement sweep: classify every row, retire the
/// work-done-and-quiet ones (stop the held process first), prune their
/// clean-and-merged worktrees, and write the receipt every removal needs to
/// stay reversible.
pub fn gc_sweep(
    home: &AgentsHome,
    emitter: &EventEmitter,
    grace_secs: i64,
    retain_days: u64,
) -> gc_sweep::GcSummary {
    // The settle writes the graph FIRST: the row pass then reads the file it
    // wrote, so a filled row reads closed and its session falls through to
    // the ordinary quiet and receipt gates. A refused settle leaves the row
    // kept under its existing reason.
    let (settled, refused) = gc_sweep::settle_stale_do_rows(home);
    let store = std::cell::RefCell::new(gc_sweep::HarnessStoreIndex::default());
    let mut summary = gc_sweep::run(
        home,
        emitter,
        grace_secs,
        false,
        retain_days,
        &gc_sweep::read_graph_entries,
        &|e| store.borrow_mut().matches(e),
        &probe_entry_ages,
        &|e| gc_sweep::stop_row_process(home, e),
        &crate::gc_native::apply_active_surface_removal,
        &crate::gc_native::apply_mux_member_retirement,
        &crate::claude_roster::read_all_agents,
        &gc_sweep::production_tree_probe,
        &crate::daemon::rm_take_worktree,
    );
    summary.settled_do_rows = settled
        .into_iter()
        .map(|row| (row.node, row.harness, row.session_id))
        .collect();
    summary.settle_refused = refused;
    summary
}

/// `fno agents reap --release <row>`: the same production seams as
/// [`gc_sweep`], with one release ruling riding the pass. The
/// settle runs first exactly as the real sweep runs it, so a do row the
/// batch can fill is already filled before the row pass reads the graph.
pub fn gc_sweep_release(
    home: &AgentsHome,
    emitter: &EventEmitter,
    grace_secs: i64,
    retain_days: u64,
    release: &gc_sweep::Release,
) -> gc_sweep::GcSummary {
    let (settled, refused) = gc_sweep::settle_stale_do_rows(home);
    let store = std::cell::RefCell::new(gc_sweep::HarnessStoreIndex::default());
    let mut summary = gc_sweep::run_with_release(
        home,
        emitter,
        grace_secs,
        false,
        retain_days,
        &gc_sweep::read_graph_entries,
        &|e| store.borrow_mut().matches(e),
        &probe_entry_ages,
        &|e| gc_sweep::stop_row_process(home, e),
        &crate::gc_native::apply_active_surface_removal,
        &crate::gc_native::apply_mux_member_retirement,
        &crate::claude_roster::read_all_agents,
        &gc_sweep::production_tree_probe,
        &crate::daemon::rm_take_worktree,
        Some(release),
    );
    summary.settled_do_rows = settled
        .into_iter()
        .map(|row| (row.node, row.harness, row.session_id))
        .collect();
    summary.settle_refused = refused;
    summary
}

/// `fno agents reap --dry-run`: classify exactly as [`gc_sweep`] does, name
/// every row under exactly one bucket, mutate nothing - a reaper an operator
/// cannot rehearse is one they will not run.
pub fn gc_sweep_dry_run(home: &AgentsHome, grace_secs: i64) -> gc_sweep::GcSummary {
    // The settle plan is read-only, and the rehearsal subtracts it from the
    // graph read so the report shows the outcome the real pass would produce.
    let mut pr_reader = crate::additional_prs::gh_pr_state_reader();
    let (planned, stamps) = crate::additional_prs::plan_settle(home, &mut pr_reader);
    let read = |h: &AgentsHome| {
        gc_sweep::read_graph_entries(h).map(|g| gc_sweep::without_settled(g, &planned, &stamps))
    };
    // Never emitted to in dry-run mode (the whole write+emit tail is skipped),
    // so an unused placeholder path satisfies the shared signature.
    let emitter = EventEmitter::new(std::path::PathBuf::new(), "daemon");
    let store = std::cell::RefCell::new(gc_sweep::HarnessStoreIndex::default());
    // The rehearsal reads the same PR states the real arm would read, so its
    // prediction holds when applied. Read-only; cached per PR per pass.
    let mut summary = gc_sweep::run(
        home,
        &emitter,
        grace_secs,
        true,
        0, // dry-run never expires: a rehearsal that pruned would not be one
        &read,
        &|e| store.borrow_mut().matches(e),
        &probe_entry_ages,
        &|e| gc_sweep::stop_row_process(home, e),
        &crate::gc_native::apply_active_surface_removal,
        &crate::gc_native::apply_mux_member_retirement,
        &crate::claude_roster::read_all_agents,
        &gc_sweep::production_tree_probe,
        &crate::daemon::rm_take_worktree,
    );
    summary.settled_do_rows = planned
        .into_iter()
        .map(|row| (row.node, row.harness, row.session_id))
        .collect();
    // The ladder's DRY-RUN plan: decisions only, no effect, no state write.
    summary.open_pr_nudge = crate::pr_nudge::plan(home, &summary.open_pr_rows, grace_secs);
    summary
}

// --- the orphan process sweep -----------------------------------------------
//
// `gc_decide` above reaps registry ROWS. Nothing reaped PROCESSES, and one was
// measured at 44.7% CPU with its parent gone: `agents stale-escalate --json`,
// reparented to init, with nothing on the machine that would ever notice it.
// Every other `libc::kill` in this crate is a targeted teardown of a pid the
// caller already owns.

/// The event the sweep emits on EVERY run, including the ones that reap
/// nothing. A reaper that speaks only when it kills cannot be told apart from a
/// reaper that never ran, and the difference is the whole question an operator
/// is asking when they go looking.
pub const ORPHAN_SWEEP_EVENT: &str = "orphan_reap_sweep";

/// How long a SIGTERMed process is given to leave before SIGKILL. Mirrors the
/// grace `cursor_agent::reap_detached_worker_servers` already uses.
const REAP_GRACE: Duration = Duration::from_secs(2);

/// One row of the process table, reduced to what the reap gate reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcRow {
    pub pid: u32,
    pub ppid: u32,
    pub age_secs: u64,
    pub args: String,
}

/// Whether this row is a child that init inherited and nobody is waiting on.
///
/// The conjunction IS the gate, and every clause is load-bearing, because the
/// cost of a false positive is killing a process a person is using:
///
/// - **argv names the `fno-py` entrypoint.** Matched on the BASENAME of a
///   whitespace-separated token, not as a substring: `--flag=fno-py-thing`
///   contains the string and is not this.
/// - **parent pid 1.** A live foreground `fno` has a real parent. Without this
///   clause the gate matches every ordinary command an operator is running.
/// - **older than the threshold.** Without it the sweep races a child whose
///   parent is mid-exit and has simply not been reaped yet.
/// - **no registry row names the pid live.** A worker the fleet is tracking is
///   not an orphan even when it looks like one from out here.
pub fn is_reapable_orphan(row: &ProcRow, older_than: Duration, live_pids: &[u32]) -> bool {
    row.ppid == 1
        && row.age_secs >= older_than.as_secs()
        && !live_pids.contains(&row.pid)
        && row.pid != std::process::id()
        && names_fno_py(&row.args)
}

fn names_fno_py(args: &str) -> bool {
    args.split_whitespace()
        .any(|token| token.rsplit('/').next() == Some("fno-py"))
}

/// Parse `ps -o etime=`: `[[DD-]HH:]MM:SS`.
///
/// `etimes` (elapsed SECONDS, no parsing) exists on Linux and NOT on macOS, so
/// the portable column is the formatted one. `None` on anything unparseable,
/// which reads as "unknown age" and therefore never reaps: an age the gate
/// cannot establish is not evidence the process is old.
pub fn parse_etime(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    let (days, rest) = match raw.split_once('-') {
        Some((d, rest)) => (d.parse::<u64>().ok()?, rest),
        None => (0, raw),
    };
    let mut parts = rest.split(':').rev();
    let secs = parts.next()?.parse::<u64>().ok()?;
    let mins = parts.next()?.parse::<u64>().ok()?;
    let hours = match parts.next() {
        Some(h) => h.parse::<u64>().ok()?,
        None => 0,
    };
    if parts.next().is_some() {
        return None;
    }
    Some(days * 86_400 + hours * 3_600 + mins * 60 + secs)
}

/// Read the process table once per sweep.
///
/// One `ps` for the whole machine, not one probe per candidate: a sweep whose
/// job is to reduce the process count must not be a fan-out of its own.
fn read_proc_table() -> Vec<ProcRow> {
    let output = match std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,etime=,args="])
        .output()
    {
        Ok(o) if o.status.success() => o.stdout,
        _ => return Vec::new(),
    };
    String::from_utf8_lossy(&output)
        .lines()
        .filter_map(parse_proc_line)
        .collect()
}

fn parse_proc_line(line: &str) -> Option<ProcRow> {
    let mut fields = line.split_whitespace();
    let pid = fields.next()?.parse().ok()?;
    let ppid = fields.next()?.parse().ok()?;
    let age_secs = parse_etime(fields.next()?)?;
    let args = fields.collect::<Vec<_>>().join(" ");
    if args.is_empty() {
        return None;
    }
    Some(ProcRow {
        pid,
        ppid,
        age_secs,
        args,
    })
}

/// Whether this pid is STILL the process the table said it was.
///
/// The gap between reading the table and signalling is small, and a pid that
/// exits inside it can be reused by something unrelated. This is a process
/// killer, so the window gets closed rather than reasoned about: re-read the
/// one row and require the same argv and the same inherited parent. An
/// unreadable answer is not a match, because absence is not evidence.
fn still_the_same(row: &ProcRow) -> bool {
    let output = match std::process::Command::new("ps")
        .args(["-p", &row.pid.to_string(), "-o", "ppid=,args="])
        .output()
    {
        Ok(o) if o.status.success() => o.stdout,
        _ => return false,
    };
    let text = String::from_utf8_lossy(&output);
    let Some(line) = text.lines().next() else {
        return false;
    };
    let mut fields = line.split_whitespace();
    let ppid: u32 = match fields.next().and_then(|f| f.parse().ok()) {
        Some(p) => p,
        None => return false,
    };
    ppid == row.ppid && fields.collect::<Vec<_>>().join(" ") == row.args
}

/// SIGTERM, then SIGKILL after the grace window. Returns false when the process
/// was already gone, changed identity, or refused the signal, so the summary
/// counts what it actually ended rather than what it aimed at.
fn terminate(row: &ProcRow) -> bool {
    let pid = row.pid;
    let alive = |p: u32| unsafe { libc::kill(p as libc::pid_t, 0) } == 0;
    if !still_the_same(row) {
        return false;
    }
    if unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } != 0 {
        return false;
    }
    let deadline = std::time::Instant::now() + REAP_GRACE;
    while alive(pid) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    if alive(pid) {
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    }
    true
}

/// Reap every `fno-py` child that init inherited and nobody is waiting on.
///
/// `live_pids` are the pids the registry vouches for. `None` means the registry
/// could not be read, and the sweep then reaps NOTHING: an unreadable registry
/// is not evidence that no worker is live, and reading it that way would turn
/// one bad file into a fleet-wide kill. It still emits, naming the refusal, so
/// a skipped run is distinguishable from a quiet one.
///
/// Emits [`ORPHAN_SWEEP_EVENT`] on every run, naming the count it reaped even
/// when that count is zero.
pub fn orphan_sweep(
    emitter: &EventEmitter,
    older_than: Duration,
    live_pids: Option<&[u32]>,
) -> usize {
    let table = read_proc_table();
    let candidates: Vec<&ProcRow> = match live_pids {
        Some(live) => table
            .iter()
            .filter(|row| is_reapable_orphan(row, older_than, live))
            .collect(),
        None => Vec::new(),
    };
    let reaped = candidates.iter().filter(|row| terminate(row)).count();
    let _ = emitter.emit(
        ORPHAN_SWEEP_EVENT,
        &serde_json::json!({
            "scanned": table.len(),
            "candidates": candidates.len(),
            "reaped": reaped,
            "older_than_secs": older_than.as_secs(),
            "skipped": live_pids.is_none(),
        }),
    );
    reaped
}

/// The pids the registry vouches for, so the sweep never ends a tracked worker.
///
/// `None` on an unreadable registry. Degrading that to an empty list would say
/// "no worker is live", which is the strongest possible licence to kill and the
/// exact opposite of what a failed read establishes.
pub fn registry_live_pids(home: &AgentsHome) -> Option<Vec<u32>> {
    crate::state::load_registry(&home.registry_json())
        .ok()
        .map(|registry| {
            registry
                .entries
                .iter()
                .flat_map(|e| [e.pid, e.keeper_child_pid])
                .flatten()
                .collect()
        })
}

const STATE_REAP_FIELDS: [&str; 5] = ["deleted", "would_delete", "kept", "bytes", "oldest_age_s"];

fn state_reap_family_tuple(family: &gc_sweep::StateReapFamilySummary) -> serde_json::Value {
    serde_json::json!([
        family.deleted,
        family.would_delete,
        family.kept.len(),
        family.bytes,
        family.oldest_age_s,
    ])
}

fn state_reap_event_payload(summary: &gc_sweep::StateFilesReapSummary) -> serde_json::Value {
    serde_json::json!({
        "fields": STATE_REAP_FIELDS,
        "families": {
            "expired_claims": state_reap_family_tuple(&summary.expired_claims),
            "plan_locks": state_reap_family_tuple(&summary.plan_locks),
            "agent_locks": state_reap_family_tuple(&summary.agent_locks),
            "pr_status_cache": state_reap_family_tuple(&summary.pr_status_cache),
            "claim_tmp": state_reap_family_tuple(&summary.claim_tmp),
        },
        "totals": [
            summary.totals.deleted,
            summary.totals.would_delete,
            summary.totals.kept,
            summary.totals.bytes,
            summary.totals.oldest_age_s,
        ],
        "skip_reason": summary.skip_reason,
    })
}

/// Apply the configured expendable-state retention policy and record one
/// bounded outcome event, including quiet and disabled passes.
pub fn state_file_sweep(
    home: &AgentsHome,
    emitter: &EventEmitter,
    cwd: &std::path::Path,
) -> gc_sweep::StateFilesReapSummary {
    let summary = gc_sweep::reap_state_files_for_cwd(
        home,
        cwd,
        crate::agents_config::state_reap_config(cwd),
        true,
    );
    let _ = emitter.emit("state_reap", &state_reap_event_payload(&summary));
    summary
}

/// The idle tick's two sweeps no registry row accounts for. `gc_sweep` retires
/// ROWS; a child whose parent died is reparented to init and nothing owned it
/// at all, and the latch's record dir is keyed by argv, so a roster whose
/// handle set changes with every worker leaves answers to questions nobody
/// asks any more. One `ps` for the whole machine, so a sweep meant to reduce
/// the process count never fans out.
pub fn unowned_sweeps(home: &AgentsHome, emitter: &EventEmitter, cwd: &std::path::Path) {
    let _ = orphan_sweep(
        emitter,
        crate::agents_config::orphan_reap_after(cwd),
        registry_live_pids(home).as_deref(),
    );
    let _ = crate::single_flight::prune_records(
        None,
        crate::agents_config::single_flight_ttl(cwd),
        crate::agents_config::single_flight_join_budget(cwd),
    );
}

/// The daemon idle tick's retirement-sweep arm, throttled to `interval`
///. Before this guard the sweep was requested every 5s tick against
/// a 900s grace, its settle pass writing the graph before it knew whether
/// there was any work: a continuous graph consumer wearing a cadence label,
/// with the one-in-flight gate ensuring the copies never stacked but never
/// slowed either. Shaped like [`crate::orphan_reap::maybe_sweep`]: elapsed
/// check, one-in-flight swap, stamp, off-loop body. The emitted `retire`
/// tick row carries the same `interval` the guard compared, so the arms
/// readout and the loop read one number.
/// The argv `fno mux workspace prune` is built from: one seam so the daemon
/// (default flags) and the manual reap verb (`--include-used-shells`) cannot
/// drift. Default flags already close an orphaned worker's tab; closing a
/// human's spent shells stays opt-in.
pub fn mux_prune_args(dry_run: bool, include_used_shells: bool) -> Vec<&'static str> {
    let mut args = vec!["mux", "workspace", "prune", "--tabs-only"];
    if include_used_shells {
        args.push("--include-used-shells");
    }
    args.push("--json");
    if dry_run {
        args.push("--dry-run");
    }
    args
}

/// (moved from the manual verb) Shell out to the existing prune verb
/// - one sweep body, reused, not reimplemented. Fail-closed: a spawn
/// failure, a non-zero exit, or an unparsable receipt is `Unread`, never a
/// measured zero.
///
/// The daemon arm passes the DEFAULT prune flags only: an orphaned worker
/// tab closes on the retire cadence (Locked Decision 6); a human's spent
/// shells stay opt-in via the manual verb.
pub fn mux_tab_sweep(dry_run: bool, include_used_shells: bool) -> crate::reap_render::MuxSweep {
    let mut cmd = std::process::Command::new(crate::scrape::fno_bin());
    cmd.args(mux_prune_args(dry_run, include_used_shells));
    match cmd.output() {
        Ok(out) => {
            let code = out.status.code();
            let stdout = String::from_utf8_lossy(&out.stdout);
            match (code, crate::reap_render::parse_prune_receipt(&stdout)) {
                (Some(0), Some(receipt)) => crate::reap_render::MuxSweep::Ran { receipt },
                (code, _) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    let stderr_first = stderr.lines().next().unwrap_or("").to_string();
                    crate::reap_render::MuxSweep::Unread {
                        exit_code: code,
                        stderr_first,
                    }
                }
            }
        }
        Err(e) => crate::reap_render::MuxSweep::Unread {
            exit_code: None,
            stderr_first: e.to_string(),
        },
    }
}

/// The production roster sweep the retire arm runs: the real enumeration
/// and removal, `dry_run` false, at the scope the caller resolved.
/// The production dead-crown sweep the retire arm runs: apply on. The
/// manual verb calls `crown_reap::sweep` itself so a dry run can report
/// without applying.
pub fn production_crown_sweep(home: &AgentsHome, cwd: &Path) -> crate::crown_reap::CrownReap {
    crate::crown_reap::production_sweep(home, cwd, true)
}

pub fn production_roster_sweep(
    home: &AgentsHome,
    grace_secs: i64,
    scope: crate::agents_config::RosterScope,
) -> crate::roster_reap::RosterReapSummary {
    crate::roster_reap::roster_reap(home, grace_secs, scope, false)
}

pub fn maybe_retirement_sweep(
    last_sweep: &mut Instant,
    in_flight: &Arc<AtomicBool>,
    next_interval: &Arc<RetireIntervalCell>,
    home: AgentsHome,
    grace_cwd: PathBuf,
    events: PathBuf,
    interval: Duration,
    tab_sweep: fn() -> crate::reap_render::MuxSweep,
    roster_sweep: fn(
        &AgentsHome,
        i64,
        crate::agents_config::RosterScope,
    ) -> crate::roster_reap::RosterReapSummary,
    crown_sweep: fn(&AgentsHome, &Path) -> crate::crown_reap::CrownReap,
) {
    if last_sweep.elapsed() < interval || in_flight.swap(true, Ordering::SeqCst) {
        return;
    }
    *last_sweep = Instant::now();
    let flag = Arc::clone(in_flight);
    let next_interval = Arc::clone(next_interval);
    tokio::task::spawn_blocking(move || {
        let _gate = crate::daemon::SweepGate(flag);
        let emitter = EventEmitter::new(events, "daemon");
        let grace_secs = crate::agents_config::retire_grace_secs(&grace_cwd) as i64;
        let retain_days = crate::agents_config::reap_receipt_retain_days(&grace_cwd);
        let _ = state_file_sweep(&home, &emitter, &grace_cwd);
        // The dead-crown sweep runs BEFORE the registry sweep: a vacated
        // crown frees the territory this tick, so the registry pass reads a
        // world that already answers for it.
        let crowns = crown_sweep(&home, &grace_cwd);
        let summary = gc_sweep(&home, &emitter, grace_secs, retain_days);
        // Locked Decision 5: the nudge ladder rides the daemon's retire arm
        // only, after the sweep that classified the open-PR rows. A manual
        // verb run never nudges; its dry run only prints the plan.
        crate::pr_nudge::run_ladder(&home, &emitter, &summary.open_pr_rows, grace_secs);
        unowned_sweeps(&home, &emitter, &grace_cwd);
        // The roster sweep runs AFTER the registry sweep: a row the registry
        // sweep retires this pass is already gone from the registry the
        // roster sweep loads. A session the roster sweep removes becomes a
        // corpse for the NEXT registry pass, through `origin_corpse`.
        let scope = crate::agents_config::roster_scope(&grace_cwd);
        let roster = roster_sweep(&home, grace_secs, scope);
        let scope_off = scope == crate::agents_config::RosterScope::Off;
        // The mux surface is one of the stores a reap must clear: the
        // default-flag prune closes an orphaned worker's tab on the retire
        // cadence, so the operator never runs the manual
        // reap verb just to clear tabs.
        let mux = tab_sweep();
        let mux_detail = match &mux {
            crate::reap_render::MuxSweep::Ran { receipt } => {
                format!("mux=ran closed={}", receipt.closed)
            }
            other => format!("mux={}", other.state()),
        };
        let roster_detail = if scope_off {
            "off".to_string()
        } else if roster.instrument_unread {
            "unreadable".to_string()
        } else {
            format!(
                "retired {} kept {} refused {}",
                roster.retired.len(),
                roster.kept.len(),
                roster.refused.len()
            )
        };
        let crowns_detail = if crowns.unread.is_some() {
            "crowns=unreadable".to_string()
        } else {
            format!(
                "crowns=vacated {} kept {}",
                crowns.vacated.len(),
                crowns.kept.len()
            )
        };
        let detail = format!(
            "roster={roster_detail} {mux_detail} {crowns_detail} held={}",
            summary.holds.len()
        );
        // `acted` counts BOTH sweeps' retirements: the registry sweep's and
        // the roster sweep's. A tick that retired only roster sessions reads
        // acted>0, never a held zero.
        let acted = summary.retired.len() + roster.retired.len();
        // A zero-acted tick says which zero it was: a sweep that could not
        // read its registry, nothing classified, or work judged and held.
        let skip_reason = if acted == 0 {
            Some(if summary.registry_unreadable {
                "registry_unreadable"
            } else if summary.kept_total() == 0 {
                "no_rows"
            } else {
                "held"
            })
        } else {
            None
        };
        // Hand back the NEXT window's interval, resolved off-loop: the tick
        // that reads it never touches config.
        let next = crate::agents_config::retire_interval_s(&grace_cwd, grace_secs.max(0) as u64);
        if let Ok(mut slot) = next_interval.lock() {
            *slot = Some(Duration::from_secs(next));
        }
        let journal = crate::loop_runtime::Journal::new_raw(
            home.events_jsonl(),
            crate::daemon::global_events_path(&home),
        );
        crate::tick_ledger::emit_tick(
            &journal,
            "retire",
            "daemon",
            acted as u64,
            skip_reason,
            Some(&detail),
            interval.as_secs(),
        );
        // AC5: every held row lands in the journal once per tick -
        // one `retire_holds` row beside the tick, so a fleet question reads
        // the event stream instead of parsing bucket counts. Zero holds
        // writes nothing.
        if !summary.holds.is_empty() {
            let _ = journal.append(
                "retire_holds",
                serde_json::json!({
                    "scheduler": "daemon",
                    "holds": summary
                        .holds
                        .iter()
                        .map(|h| {
                            serde_json::json!({
                                "id": h.id, "reason": h.reason, "detail": h.detail,
                                "age_s": h.age_s, "escalated": h.escalated,
                            })
                        })
                        .collect::<Vec<_>>(),
                }),
            );
        }
    });
}

#[cfg(test)]
mod tests {
    fn no_agents() -> crate::claude_roster::ClaudeAgentsSnapshot {
        crate::claude_roster::ClaudeAgentsSnapshot::unknown("test: no snapshot staged")
    }

    /// The roster-seam stub every retire-arm test passes: the arm wiring is
    /// under test, never the sweep body, and no unit test shells out to the
    /// live claude roster.
    fn noop_roster_sweep(
        _home: &AgentsHome,
        _grace_secs: i64,
        _scope: crate::agents_config::RosterScope,
    ) -> crate::roster_reap::RosterReapSummary {
        crate::roster_reap::RosterReapSummary::default()
    }

    /// Same stub for the crown seam: the arm wiring is under test, never
    /// the dead-crown sweep body.
    fn noop_crown_sweep(_home: &AgentsHome, _cwd: &Path) -> crate::crown_reap::CrownReap {
        crate::crown_reap::CrownReap::default()
    }

    use super::*;

    // --- the retirement sweep arm ---

    fn retirement_sweep_tmp_home(tag: &str) -> (std::path::PathBuf, AgentsHome) {
        let dir = std::env::temp_dir().join(format!(
            "fno-retire-arm-{}-{}-{tag}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = AgentsHome::at(&dir);
        home.ensure_root().unwrap();
        crate::paths::pin_test_claims_root(&dir.join("claims-root"));
        (dir, home)
    }

    fn count_retire_rows(path: &std::path::Path) -> usize {
        // Committed rows, not journal bytes: the store cutover stopped journal
        // appends, so emitted ticks live only in the store beside the journal.
        let _ = crate::event_store::import_all(path);
        crate::event_store::query_events(
            path,
            &crate::event_store::EventQuery {
                types: vec!["control_plane_tick".to_string()],
                ..Default::default()
            },
        )
        .unwrap_or_default()
        .iter()
        .filter(|r| {
            serde_json::from_str::<serde_json::Value>(&r.line)
                .ok()
                .and_then(|row| {
                    row.get("data")
                        .and_then(|d| d.get("arm"))
                        .and_then(serde_json::Value::as_str)
                        .map(|arm| arm == "retire")
                })
                .unwrap_or(false)
        })
        .count()
    }

    fn wait_for_retire_row(path: &std::path::Path) -> usize {
        wait_for_retire_row_within(path, 5)
    }

    /// The longer bound for registries whose rows the age seam must probe
    /// through a real subprocess: an adopted claude row in a sandbox has no
    /// transcript store, so the probe runs out its whole timeout before the
    /// sweep classifies and the tick lands.
    fn wait_for_retire_row_within(path: &std::path::Path, secs: u64) -> usize {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let n = count_retire_rows(path);
            if n >= 1 {
                return n;
            }
            assert!(
                Instant::now() < deadline,
                "retire tick row never landed in {}",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn retirement_sweep_guard_admits_one_run_per_window() {
        let (dir, home) = retirement_sweep_tmp_home("one-run");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let in_flight = Arc::new(AtomicBool::new(false));
            let cell: Arc<RetireIntervalCell> = Arc::new(Mutex::new(None));
            // Backdate the stamp: a fresh stamp means the guard correctly
            // waits out the window, so the test starts due like the daemon
            // is one interval after boot.
            let mut last = Instant::now() - Duration::from_secs(301);
            let grace_cwd = dir.clone();
            // The interval is passed straight through (no resolution), so
            // this test is independent of machine config.
            crate::gc::maybe_retirement_sweep(
                &mut last,
                &in_flight,
                &cell,
                home.clone(),
                grace_cwd.clone(),
                home.events_jsonl(),
                Duration::from_secs(300),
                || crate::reap_render::MuxSweep::Skipped,
                noop_roster_sweep,
                noop_crown_sweep,
            );
            // A second tick inside the window is refused by the elapsed
            // check: no second run can start until the window closes.
            crate::gc::maybe_retirement_sweep(
                &mut last,
                &in_flight,
                &cell,
                home.clone(),
                grace_cwd,
                home.events_jsonl(),
                Duration::from_secs(300),
                || crate::reap_render::MuxSweep::Skipped,
                noop_roster_sweep,
                noop_crown_sweep,
            );
            let rows = wait_for_retire_row(&home.events_jsonl());
            assert_eq!(rows, 1, "two ticks in one window must yield one sweep");
            // Give a still-running first sweep a moment, then confirm no
            // second row appeared behind it.
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert_eq!(
                count_retire_rows(&home.events_jsonl()),
                1,
                "no second run may land inside one guard window"
            );
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retirement_sweep_tick_row_reports_the_interval_the_loop_enforces() {
        // Serialize env mutation against the resolver tests in agents_config
        // (one shared crate-wide env lock).
        let _g = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("FNO_AGENTS_RETIRE_INTERVAL_SECS", "25");
        let (dir, home) = retirement_sweep_tmp_home("interval-readback");
        let grace_cwd = dir.clone();
        // The interval a previous window handed back, as the daemon arm
        // would read it, and the value the body re-resolves into the cell.
        let grace = crate::agents_config::retire_grace_secs(&grace_cwd);
        let interval =
            Duration::from_secs(crate::agents_config::retire_interval_s(&grace_cwd, grace));
        assert!(interval.as_secs() >= 5, "interval floor is the 5s tick");
        let cell: Arc<RetireIntervalCell> = Arc::new(Mutex::new(Some(interval)));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let in_flight = Arc::new(AtomicBool::new(false));
            let mut last = Instant::now() - Duration::from_secs(interval.as_secs() + 1);
            crate::gc::maybe_retirement_sweep(
                &mut last,
                &in_flight,
                &cell,
                home.clone(),
                grace_cwd.clone(),
                home.events_jsonl(),
                interval,
                || crate::reap_render::MuxSweep::Skipped,
                noop_roster_sweep,
                noop_crown_sweep,
            );
            wait_for_retire_row(&home.events_jsonl());
            let row = crate::events::committed_journal_text(&home.events_jsonl())
                .lines()
                .filter(|l| {
                    l.contains("\"type\":\"control_plane_tick\"")
                        && l.contains("\"arm\":\"retire\"")
                })
                .last()
                .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
                .expect("row read back");
            assert_eq!(
                row["data"]["interval_s"],
                interval.as_secs(),
                "the arms row must carry the interval the guard compared"
            );
            // The body handed back the NEXT window's interval, resolved
            // under the same env: the handoff loop is closed.
            assert_eq!(
                retire_interval_snapshot(&cell),
                interval,
                "the body must hand back the resolved interval"
            );
        });
        std::env::remove_var("FNO_AGENTS_RETIRE_INTERVAL_SECS");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retirement_sweep_seed_honors_the_configured_cadence() {
        // Boot seeds the cell with the CONFIGURED interval, so the first
        // window after a daemon start waits the configured time, not the
        // default.
        let _g = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("FNO_AGENTS_RETIRE_INTERVAL_SECS", "45");
        let (dir, _home) = retirement_sweep_tmp_home("seed");
        let cell = crate::gc::seed_retire_interval_cell(&dir);
        let grace = crate::agents_config::retire_grace_secs(&dir);
        let expected = Duration::from_secs(crate::agents_config::retire_interval_s(&dir, grace));
        std::env::remove_var("FNO_AGENTS_RETIRE_INTERVAL_SECS");
        assert_eq!(retire_interval_snapshot(&cell), expected);
        assert_eq!(retire_interval_snapshot(&cell).as_secs(), 45);
    }

    /// One retirement pass against a temp home, through the real spawn
    /// path, returning the tick row it landed. The tab and roster sweeps
    /// arrive as seams, so no test reaches the shared mux or the live
    /// claude roster.
    fn run_retire_pass_and_read_tick(
        dir: &std::path::Path,
        home: &AgentsHome,
        tab_sweep: fn() -> crate::reap_render::MuxSweep,
        roster_sweep: fn(
            &AgentsHome,
            i64,
            crate::agents_config::RosterScope,
        ) -> crate::roster_reap::RosterReapSummary,
    ) -> serde_json::Value {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let in_flight = Arc::new(AtomicBool::new(false));
            let cell: Arc<RetireIntervalCell> = Arc::new(Mutex::new(None));
            let mut last = Instant::now() - Duration::from_secs(301);
            crate::gc::maybe_retirement_sweep(
                &mut last,
                &in_flight,
                &cell,
                home.clone(),
                dir.to_path_buf(),
                home.events_jsonl(),
                Duration::from_secs(300),
                tab_sweep,
                roster_sweep,
                noop_crown_sweep,
            );
            // The production seams probe staged rows through real
            // subprocesses; in a sandbox without a transcript store those
            // probes run out their whole timeout before the tick lands.
            wait_for_retire_row_within(&home.events_jsonl(), 30);
            crate::events::committed_journal_text(&home.events_jsonl())
                .lines()
                .filter(|l| {
                    l.contains("\"type\":\"control_plane_tick\"")
                        && l.contains("\"arm\":\"retire\"")
                })
                .last()
                .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
                .expect("row read back")
        })
    }

    #[test]
    fn retire_tick_names_the_tabs_the_sweep_closed() {
        // AC5-HP: the tab sweep rides the retire pass and the tick's detail
        // carries its receipt; the default argv carries no used-shells flag.
        let (dir, home) = retirement_sweep_tmp_home("mux-ran");
        let row = run_retire_pass_and_read_tick(
            &dir,
            &home,
            || crate::reap_render::MuxSweep::Ran {
                receipt: crate::reap_render::PruneReceipt {
                    closed: 2,
                    would_close: 0,
                    close_named: vec!["target-x-1-a".to_string(), "target-x-1-b".to_string()],
                    sessions_unreachable: Vec::new(),
                    notice: None,
                },
            },
            noop_roster_sweep,
        );
        assert_eq!(row["data"]["acted"], 0);
        assert_eq!(row["data"]["skip_reason"], "no_rows");
        assert!(
            row["data"]["detail"]
                .as_str()
                .unwrap()
                .contains("mux=ran closed=2"),
            "the tick must name the tab sweep's receipt: {:?}",
            row["data"]["detail"]
        );
        assert_eq!(
            mux_prune_args(false, false),
            vec!["mux", "workspace", "prune", "--tabs-only", "--json"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retire_tick_survives_an_unread_tab_sweep() {
        // AC5-UNREAD: a tab sweep that cannot be read never eats the tick;
        // the detail says mux=unread.
        let (dir, home) = retirement_sweep_tmp_home("mux-unread");
        let row = run_retire_pass_and_read_tick(
            &dir,
            &home,
            || crate::reap_render::MuxSweep::Unread {
                exit_code: Some(1),
                stderr_first: "boom".to_string(),
            },
            noop_roster_sweep,
        );
        assert_eq!(
            row["data"]["skip_reason"], "no_rows",
            "an empty registry with an unread mux still classified no row"
        );
        assert!(
            row["data"]["detail"]
                .as_str()
                .unwrap()
                .contains("mux=unread"),
            "the tick must name the unread sweep: {:?}",
            row["data"]["detail"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Where the recording stub reads and writes the scope the arm passed.
    static ROSTER_SEEN_SCOPE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(255);

    /// A roster seam that records the scope the arm resolved, so the test
    /// reads back what the arm handed the sweep.
    fn recording_roster_sweep(
        _home: &AgentsHome,
        _grace_secs: i64,
        scope: crate::agents_config::RosterScope,
    ) -> crate::roster_reap::RosterReapSummary {
        use crate::agents_config::RosterScope as S;
        ROSTER_SEEN_SCOPE.store(
            match scope {
                S::Off => 0,
                S::Provenanced => 1,
                S::All => 2,
            },
            std::sync::atomic::Ordering::SeqCst,
        );
        crate::roster_reap::RosterReapSummary::default()
    }

    /// A roster seam whose only keeps read `roster unreadable`, the
    /// enumeration-failed shape the arm must name on the tick.
    fn unreadable_roster_sweep(
        _home: &AgentsHome,
        _grace_secs: i64,
        _scope: crate::agents_config::RosterScope,
    ) -> crate::roster_reap::RosterReapSummary {
        crate::roster_reap::RosterReapSummary {
            instrument_unread: true,
            kept: vec![crate::roster_reap::RosterJudgement {
                short_id: String::new(),
                node: None,
                reason: "roster unreadable: test stub".to_string(),
                retired: false,
            }],
            ..Default::default()
        }
    }

    /// AC7-HP: the retire arm runs the roster sweep after the registry
    /// sweep, hands it the resolved scope, and the tick detail carries the
    /// roster counts beside the mux line.
    #[test]
    fn the_retire_arm_runs_the_roster_sweep_after_the_registry_sweep() {
        let _env = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let old = std::env::var_os("FNO_CONFIG");
        std::env::remove_var("FNO_CONFIG");
        let (dir, home) = retirement_sweep_tmp_home("roster-arm");
        std::fs::create_dir_all(dir.join(".fno")).unwrap();
        std::fs::write(
            dir.join(".fno/config.toml"),
            "[agents.reap]\nroster_scope = \"provenanced\"\n",
        )
        .unwrap();
        let row = run_retire_pass_and_read_tick(
            &dir,
            &home,
            || crate::reap_render::MuxSweep::Skipped,
            recording_roster_sweep,
        );
        assert_eq!(
            ROSTER_SEEN_SCOPE.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the arm passes the resolved scope (provenanced here)"
        );
        assert!(
            row["data"]["detail"]
                .as_str()
                .unwrap()
                .contains("roster=retired 0 kept 0 refused 0"),
            "{:?}",
            row["data"]["detail"]
        );
        match &old {
            Some(h) => std::env::set_var("FNO_CONFIG", h),
            None => std::env::remove_var("FNO_CONFIG"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AC8-ERR: with `agents.reap.roster_scope = "off"` the arm resolves the
    /// off scope, hands it to the sweep, and the tick reads `roster=off`.
    #[test]
    fn roster_scope_off_writes_roster_off() {
        let _env = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let old = std::env::var_os("FNO_CONFIG");
        std::env::remove_var("FNO_CONFIG");
        let (dir, home) = retirement_sweep_tmp_home("roster-off");
        std::fs::create_dir_all(dir.join(".fno")).unwrap();
        std::fs::write(
            dir.join(".fno/config.toml"),
            "[agents.reap]\nroster_scope = \"off\"\n",
        )
        .unwrap();
        let row = run_retire_pass_and_read_tick(
            &dir,
            &home,
            || crate::reap_render::MuxSweep::Skipped,
            recording_roster_sweep,
        );
        assert_eq!(
            ROSTER_SEEN_SCOPE.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the arm passes the off scope through to the seam"
        );
        assert!(
            row["data"]["detail"]
                .as_str()
                .unwrap()
                .contains("roster=off"),
            "{:?}",
            row["data"]["detail"]
        );
        match &old {
            Some(h) => std::env::set_var("FNO_CONFIG", h),
            None => std::env::remove_var("FNO_CONFIG"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AC9-EDGE: a roster sweep whose enumeration failed removes nothing,
    /// leaves the registry sweep's verdict unchanged, and the tick reads
    /// `roster=unreadable`.
    #[test]
    fn an_unreadable_roster_removes_nothing() {
        let (dir, home) = retirement_sweep_tmp_home("roster-unreadable");
        let row = run_retire_pass_and_read_tick(
            &dir,
            &home,
            || crate::reap_render::MuxSweep::Skipped,
            unreadable_roster_sweep,
        );
        assert_eq!(row["data"]["acted"], 0);
        assert_eq!(row["data"]["skip_reason"], "no_rows");
        assert!(
            row["data"]["detail"]
                .as_str()
                .unwrap()
                .contains("roster=unreadable"),
            "{:?}",
            row["data"]["detail"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retire_tick_says_held_when_it_kept_a_row() {
        // AC5-SKIP: a pass that judged one row and kept it reports `held`,
        // not the indistinguishable zero; `no_rows` stays the no-classified
        // reading.
        let (dir, home) = retirement_sweep_tmp_home("mux-held");
        let registry = serde_json::json!({
            "schema_version": 10,
            "agents": [{
                "name": "target-x-1-adopted",
                "cwd": dir.display().to_string(),
                "status": "exited",
                "created_at": "2026-09-06T00:00:00Z",
                "harness": "claude",
                "harness_session_id": "sess-adopted",
                "short_id": "abc123",
                "origin": "adopted",
            }],
        });
        std::fs::create_dir_all(home.root()).unwrap();
        std::fs::write(
            home.registry_json(),
            serde_json::to_string(&registry).unwrap(),
        )
        .unwrap();
        let row = run_retire_pass_and_read_tick(
            &dir,
            &home,
            || crate::reap_render::MuxSweep::Skipped,
            noop_roster_sweep,
        );
        assert_eq!(row["data"]["acted"], 0);
        assert_eq!(
            row["data"]["skip_reason"], "held",
            "one judged-and-kept row is a hold, not a bare zero: {:?}",
            row["data"]
        );
        assert!(
            row["data"]["detail"]
                .as_str()
                .unwrap()
                .contains("mux=skipped"),
            "the daemon stub skips the mux: {:?}",
            row["data"]["detail"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unreadable_registry_renders_a_failure_not_no_rows() {
        // A registry read that fails is not a census of zero: the tick names
        // registry_unreadable, a FAILURE_SKIPS token, so the arms readout
        // renders FAIL instead of a quiet ok.
        let (dir, home) = retirement_sweep_tmp_home("registry-unreadable");
        std::fs::create_dir_all(home.root()).unwrap();
        std::fs::write(home.registry_json(), "{not json").unwrap();
        let row = run_retire_pass_and_read_tick(
            &dir,
            &home,
            || crate::reap_render::MuxSweep::Skipped,
            noop_roster_sweep,
        );
        assert_eq!(row["data"]["acted"], 0);
        assert_eq!(row["data"]["skip_reason"], "registry_unreadable");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn manual_reap_verb_argv_includes_used_shells() {
        // AC5-CLI: the manual verb opts in to closing a human's spent
        // shells; the daemon default never does.
        assert_eq!(
            mux_prune_args(false, true),
            vec![
                "mux",
                "workspace",
                "prune",
                "--tabs-only",
                "--include-used-shells",
                "--json"
            ]
        );
        assert_eq!(
            mux_prune_args(true, true),
            vec![
                "mux",
                "workspace",
                "prune",
                "--tabs-only",
                "--include-used-shells",
                "--json",
                "--dry-run"
            ]
        );
    }

    /// AC5-HP: a tick whose sweep held rows writes ONE
    /// `retire_holds` journal row naming each held id, beside the tick row.
    #[test]
    fn a_tick_with_held_rows_writes_one_retire_holds_event() {
        let _env = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let dir = tempfile::tempdir().unwrap();
        // The agents home sits UNDER dir so the production graph read
        // (home.root().parent()/graph.json) answers dir/graph.json.
        let home = AgentsHome::at(dir.path().join("agents"));
        home.ensure_root().unwrap();
        crate::paths::pin_test_claims_root(dir.path());
        std::fs::write(
            dir.path().join("graph.json"),
            serde_json::to_vec(&serde_json::json!({
                "entries": [{
                    "id": "x-h1",
                    "status": "idea",
                    "project": "p",
                    "sessions": [{
                        "phase": "blueprint",
                        "harness": "codex",
                        "session_id": "s-h1",
                        "started_at": "2026-09-01T00:00:00Z",
                    }],
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        crate::state::update_registry(&home.registry_json(), |r| {
            let mut e = crate::state::RegistryEntry::default();
            e.name = "bp-x-h1-a".into();
            e.short_id = "bp-x-h1-a".into();
            e.origin = Some("spawn".into());
            e.harness = Some("codex".into());
            e.harness_session_id = Some("s-h1".into());
            e.created_at = "2026-09-01T00:00:00Z".into();
            e.status = crate::AgentStatus::Exited;
            r.entries.push(e);
        })
        .unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let in_flight = Arc::new(AtomicBool::new(false));
            let cell: Arc<RetireIntervalCell> = Arc::new(Mutex::new(None));
            let mut last = Instant::now() - Duration::from_secs(301);
            crate::gc::maybe_retirement_sweep(
                &mut last,
                &in_flight,
                &cell,
                home.clone(),
                dir.path().to_path_buf(),
                home.events_jsonl(),
                Duration::from_secs(300),
                || crate::reap_render::MuxSweep::Skipped,
                noop_roster_sweep,
                noop_crown_sweep,
            );
            // The production age probe pays a real subprocess on this
            // fixture (two probes, seconds apiece under load), so the tick
            // can land long past the 5 s deadline the empty-home tests use.
            wait_for_line(&home.events_jsonl(), "\"arm\":\"retire\"", 90);
            wait_for_line(&home.events_jsonl(), "\"type\":\"retire_holds\"", 90);
        });
        let holds_row = crate::events::committed_journal_text(&home.events_jsonl())
            .lines()
            .find(|l| l.contains("\"type\":\"retire_holds\""))
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .expect("one retire_holds row for the held planner");
        let holds = holds_row["data"]["holds"].as_array().expect("holds list");
        assert_eq!(holds.len(), 1, "{holds_row}");
        assert_eq!(holds[0]["id"], "bp-x-h1-a");
        assert_eq!(
            holds[0]["reason"],
            "planning assignment not finished by this session"
        );
        assert_eq!(holds_row["data"]["scheduler"], "daemon");
        let _ = std::fs::remove_dir_all(dir.path());
    }

    /// AC5-EDGE: a tick with zero holds writes NO `retire_holds`
    /// row, even when the pass kept a row under a hold-free bucket.
    #[test]
    fn a_tick_with_zero_holds_writes_no_retire_holds_event() {
        let _env = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (dir, home) = retirement_sweep_tmp_home("no-holds");
        let registry = serde_json::json!({
            "schema_version": 10,
            "agents": [{
                "name": "target-x-1-adopted",
                "cwd": dir.display().to_string(),
                "status": "exited",
                "created_at": "2026-09-06T00:00:00Z",
                "harness": "claude",
                "harness_session_id": "sess-adopted",
                "short_id": "abc123",
                "origin": "adopted",
            }],
        });
        std::fs::create_dir_all(home.root()).unwrap();
        std::fs::write(
            home.registry_json(),
            serde_json::to_string(&registry).unwrap(),
        )
        .unwrap();
        run_retire_pass_and_read_tick(
            &dir,
            &home,
            || crate::reap_render::MuxSweep::Skipped,
            noop_roster_sweep,
        );
        let count = crate::events::committed_journal_text(&home.events_jsonl())
            .lines()
            .filter(|l| l.contains("\"type\":\"retire_holds\""))
            .count();
        assert_eq!(count, 0, "no holds, no journal row");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Poll for a journal line until `secs` elapse (the retire_holds row is
    /// appended right after the tick row; the append is a plain write).
    fn wait_for_line(path: &std::path::Path, needle: &str, secs: u64) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if crate::events::committed_journal_text(path).contains(needle) {
                return;
            }
            if Instant::now() >= deadline {
                panic!("line never landed in {path:?}: {needle}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn state_reap_event_reports_counts() {
        let _env = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = tempfile::tempdir().unwrap();
        let prior_claims = std::env::var_os("FNO_CLAIMS_ROOT");
        let prior_home = std::env::var_os("HOME");
        let prior_pr_cache = std::env::var_os("FNO_PR_STATUS_CACHE_DIR");
        std::env::set_var("FNO_CLAIMS_ROOT", root.path());
        std::env::set_var("HOME", root.path());
        std::env::set_var(
            "FNO_PR_STATUS_CACHE_DIR",
            root.path().join(".fno/cache/pr-status"),
        );
        let home = AgentsHome::at(root.path().join("agents"));
        home.ensure_root().unwrap();
        let cwd = root.path().join("repo");
        std::fs::create_dir_all(cwd.join(".fno")).unwrap();
        std::fs::write(
            cwd.join(".fno/config.toml"),
            "[agents.state_reap]\n\
             enabled = true\n\
             locks_retain_days = 1\n\
             expired_claims_retain_days = 1\n\
             pr_status_cache_retain_days = 1\n",
        )
        .unwrap();
        let claim = root.path().join(".fno/claims/.expired/old-claim");
        std::fs::create_dir_all(claim.parent().unwrap()).unwrap();
        std::fs::write(&claim, b"claim").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 86_400);
        std::fs::File::options()
            .write(true)
            .open(&claim)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();

        let emitter = EventEmitter::new(home.events_jsonl(), "test");
        let summary = state_file_sweep(&home, &emitter, &cwd);

        assert_eq!(summary.expired_claims.deleted, 1);
        assert!(!claim.exists());
        let quiet = state_file_sweep(&home, &emitter, &cwd);
        assert_eq!(quiet.totals.scanned, 0);
        let lines: Vec<serde_json::Value> =
            crate::events::committed_journal_text(&home.events_jsonl())
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
        assert_eq!(lines.len(), 2, "each periodic pass must emit one event");
        let event = &lines[0];
        assert_eq!(event["type"], "state_reap");
        assert_ne!(event["type"], "event_payload_too_large");
        assert_eq!(event["source"], "test");
        let data = &event["data"];
        assert_eq!(
            data["fields"],
            serde_json::json!(["deleted", "would_delete", "kept", "bytes", "oldest_age_s"])
        );
        for family in [
            "expired_claims",
            "plan_locks",
            "agent_locks",
            "pr_status_cache",
        ] {
            assert_eq!(
                data["families"][family].as_array().map(Vec::len),
                Some(5),
                "missing compact {family} tuple: {data}"
            );
        }
        assert_eq!(data["families"]["expired_claims"][0], 1);
        assert_eq!(data["families"]["expired_claims"][1], 0);
        assert_eq!(data["families"]["expired_claims"][2], 0);
        assert_eq!(data["families"]["expired_claims"][3], 5);
        assert!(data["families"]["expired_claims"][4].is_number());
        assert_eq!(data["totals"][0], 1);
        assert_eq!(data["totals"][1], 0);
        assert_eq!(data["totals"][2], 0);
        assert_eq!(data["totals"][3], 5);
        assert!(data["totals"][4].is_number());
        assert!(data["skip_reason"].is_null());
        assert_eq!(lines[1]["type"], "state_reap");
        assert_eq!(
            lines[1]["data"]["totals"],
            serde_json::json!([0, 0, 0, 0, null])
        );
        assert!(lines[1]["data"]["skip_reason"].is_null());

        let mut live = gc_sweep::StateFilesReapSummary::default();
        live.expired_claims.deleted = 6_594;
        live.expired_claims.would_delete = 2_393;
        live.expired_claims.kept = vec![
            gc_sweep::StateReapKept {
                path: String::new(),
                reason: String::new(),
            };
            27
        ];
        live.expired_claims.bytes = 9_880_000;
        live.expired_claims.oldest_age_s = Some(8_631_360);
        live.plan_locks.deleted = 2_393;
        live.agent_locks.deleted = 4_344;
        live.pr_status_cache.deleted = 123;
        live.totals.deleted = 13_454;
        live.totals.would_delete = 2_393;
        live.totals.kept = 27;
        live.totals.bytes = 9_880_000;
        live.totals.oldest_age_s = Some(8_631_360);
        let payload = state_reap_event_payload(&live);
        let payload_len = serde_json::to_vec(&payload).unwrap().len();
        assert!(
            payload_len <= crate::events_limits::max_data_bytes(),
            "live-sized state_reap payload is {payload_len}B: {payload}"
        );
        match prior_claims {
            Some(value) => std::env::set_var("FNO_CLAIMS_ROOT", value),
            None => std::env::remove_var("FNO_CLAIMS_ROOT"),
        }
        match prior_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        match prior_pr_cache {
            Some(value) => std::env::set_var("FNO_PR_STATUS_CACHE_DIR", value),
            None => std::env::remove_var("FNO_PR_STATUS_CACHE_DIR"),
        }
    }

    #[test]
    fn state_reap_event_reports_disabled_pass() {
        let root = tempfile::tempdir().unwrap();
        let home = AgentsHome::at(root.path().join("agents"));
        home.ensure_root().unwrap();
        crate::paths::pin_test_claims_root(root.path());
        let cwd = root.path().join("repo");
        std::fs::create_dir_all(cwd.join(".fno")).unwrap();
        std::fs::write(
            cwd.join(".fno/config.toml"),
            "[agents.state_reap]\nenabled = false\n",
        )
        .unwrap();

        let emitter = EventEmitter::new(home.events_jsonl(), "test");
        let summary = state_file_sweep(&home, &emitter, &cwd);

        assert_eq!(summary.skip_reason.as_deref(), Some("disabled"));
        let raw = crate::events::committed_journal_text(&home.events_jsonl());
        let event: serde_json::Value = serde_json::from_str(raw.trim()).unwrap();
        assert_eq!(event["type"], "state_reap");
        assert_ne!(event["type"], "event_payload_too_large");
        assert_eq!(
            event["data"]["fields"],
            serde_json::json!(["deleted", "would_delete", "kept", "bytes", "oldest_age_s"])
        );
        for family in [
            "expired_claims",
            "plan_locks",
            "agent_locks",
            "pr_status_cache",
        ] {
            assert_eq!(
                event["data"]["families"][family],
                serde_json::json!([0, 0, 0, 0, null])
            );
        }
        assert_eq!(
            event["data"]["totals"],
            serde_json::json!([0, 0, 0, 0, null])
        );
        assert_eq!(event["data"]["skip_reason"], "disabled");
    }

    // --- the orphan process sweep ---

    fn orphan(args: &str, ppid: u32, age_secs: u64) -> ProcRow {
        ProcRow {
            pid: 4242,
            ppid,
            age_secs,
            args: args.to_string(),
        }
    }

    const FNO_PY: &str = "/Users/x/.local/share/uv/tools/fno/bin/python3 \
/Users/x/.local/share/uv/tools/fno/bin/fno-py agents stale-escalate --json";

    /// AC11: every clause satisfied is the only shape that reaps.
    #[test]
    fn an_inherited_past_threshold_fno_child_is_reapable() {
        let row = orphan(FNO_PY, 1, 7200);
        assert!(is_reapable_orphan(&row, Duration::from_secs(5400), &[]));
    }

    /// AC12: without the age bound the sweep races a child whose parent is
    /// mid-exit and has simply not been reaped yet.
    #[test]
    fn a_young_inherited_child_is_left_alone() {
        let row = orphan(FNO_PY, 1, 60);
        assert!(!is_reapable_orphan(&row, Duration::from_secs(5400), &[]));
    }

    /// AC13: a live parent means somebody is waiting on it, however old it is.
    #[test]
    fn a_child_with_a_living_parent_is_left_alone() {
        let row = orphan(FNO_PY, 99_000, 86_400);
        assert!(!is_reapable_orphan(&row, Duration::from_secs(5400), &[]));
    }

    /// A worker the registry vouches for is not an orphan, whatever the process
    /// table says: the fleet is tracking it.
    #[test]
    fn a_pid_the_registry_names_live_is_left_alone() {
        let row = orphan(FNO_PY, 1, 86_400);
        assert!(!is_reapable_orphan(
            &row,
            Duration::from_secs(5400),
            &[4242]
        ));
    }

    /// The argv clause matches a BASENAME, not a substring. A flag that merely
    /// contains the string is not the entrypoint, and killing on it would end
    /// somebody's foreground command.
    #[test]
    fn only_the_entrypoint_basename_counts_as_the_marker() {
        assert!(!is_reapable_orphan(
            &orphan("/usr/bin/grep --include=fno-py-notes .", 1, 86_400),
            Duration::from_secs(5400),
            &[]
        ));
        assert!(!is_reapable_orphan(
            &orphan("/usr/bin/vim notes.txt", 1, 86_400),
            Duration::from_secs(5400),
            &[]
        ));
        assert!(is_reapable_orphan(
            &orphan("fno-py backlog advance --json", 1, 86_400),
            Duration::from_secs(5400),
            &[]
        ));
    }

    /// `etimes` does not exist on macOS, so the portable column is the
    /// formatted one and this parser is what stands between it and the gate.
    #[test]
    fn etime_parses_every_shape_ps_prints() {
        assert_eq!(parse_etime("53:30"), Some(3210));
        assert_eq!(parse_etime("01:08:49"), Some(4129));
        assert_eq!(
            parse_etime("  20701-11:56:27  "),
            Some(20701 * 86_400 + 11 * 3_600 + 56 * 60 + 27)
        );
        assert_eq!(parse_etime("2-00:00:00"), Some(172_800));
    }

    /// An age the parser cannot establish reads as unknown, and unknown never
    /// reaps: a failed read is not evidence that a process is old.
    #[test]
    fn an_unparseable_age_never_reaps() {
        assert_eq!(parse_etime("banana"), None);
        assert_eq!(parse_etime(""), None);
        assert_eq!(parse_etime("1:2:3:4"), None);
        assert!(parse_proc_line("100 1 banana fno-py do pr wait 1").is_none());
    }

    /// An unreadable registry is not evidence that no worker is live. Reading
    /// it that way would turn one bad file into a fleet-wide kill.
    #[test]
    fn an_unreadable_registry_reaps_nothing_and_says_so() {
        let dir = std::env::temp_dir().join(format!("fno-orphan-skip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.jsonl");
        let _ = std::fs::remove_file(&path);
        let emitter = EventEmitter::new(path.clone(), "daemon");
        // A threshold of zero would reap every orphan on the box; None must
        // hold it back anyway.
        let reaped = orphan_sweep(&emitter, Duration::from_secs(0), None);
        assert_eq!(reaped, 0);
        // orphan_reap_sweep is ephemeral-class: the store keeps the row in the
        // same journal with retention_class ephemeral; the sibling is never
        // created.
        let rows =
            crate::event_store::query_events(&path, &crate::event_store::EventQuery::default())
                .unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].retention_class, "ephemeral");
        assert!(
            rows[0].line.contains("\"skipped\":true"),
            "{}",
            rows[0].line
        );
        assert!(
            rows[0].line.contains("\"candidates\":0"),
            "{}",
            rows[0].line
        );
        assert!(!PathBuf::from(format!(
            "{}{}",
            path.display(),
            crate::event_store::EPHEMERAL_SUFFIX
        ))
        .exists());
    }

    /// A pid that changed identity between the table read and the signal is
    /// somebody else's process now.
    #[test]
    fn a_pid_that_changed_identity_is_not_signalled() {
        // This process is alive and is not what the row claims, so the
        // re-verification must refuse it.
        let row = ProcRow {
            pid: std::process::id(),
            ppid: 1,
            age_secs: 86_400,
            args: "/tools/fno/bin/fno-py agents truth --handles a".to_string(),
        };
        assert!(!still_the_same(&row));
        assert!(!terminate(&row));
    }

    /// The same pid read honestly matches itself, so the guard is not simply
    /// refusing everything - the positive control for the check above.
    #[test]
    fn the_guard_matches_a_row_read_from_the_live_table() {
        let me = std::process::id();
        let row = read_proc_table()
            .into_iter()
            .find(|r| r.pid == me)
            .expect("this process must appear in its own process table");
        assert!(still_the_same(&row));
    }

    /// AC14: the sweep speaks on every run. A reaper that only speaks when it
    /// kills cannot be told apart from a reaper that never ran, and the done
    /// probe depends on the difference.
    #[test]
    fn a_sweep_that_reaps_nothing_still_emits() {
        let dir = std::env::temp_dir().join(format!("fno-orphan-sweep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.jsonl");
        let _ = std::fs::remove_file(&path);
        let emitter = EventEmitter::new(path.clone(), "daemon");
        // A threshold no live process can reach, so the sweep finds nothing.
        let reaped = orphan_sweep(&emitter, Duration::from_secs(u32::MAX as u64), Some(&[]));
        assert_eq!(reaped, 0);
        // Same store routing as above: the sweep row is committed with
        // retention_class ephemeral and no sibling journal is ever created.
        let rows =
            crate::event_store::query_events(&path, &crate::event_store::EventQuery::default())
                .unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].retention_class, "ephemeral");
        assert!(
            rows[0].line.contains(ORPHAN_SWEEP_EVENT),
            "{}",
            rows[0].line
        );
        assert!(rows[0].line.contains("\"reaped\":0"), "{}", rows[0].line);
        assert!(!PathBuf::from(format!(
            "{}{}",
            path.display(),
            crate::event_store::EPHEMERAL_SUFFIX
        ))
        .exists());
    }

    /// The process table read is one `ps` for the whole machine: a sweep whose
    /// job is to reduce the process count must not fan out itself.
    #[test]
    fn the_process_table_read_parses_a_real_ps_line() {
        let row = parse_proc_line(
            "60057 60045 08:34 /tools/fno/bin/python3 /tools/fno/bin/fno-py do pr wait 1483",
        )
        .unwrap();
        assert_eq!(row.pid, 60057);
        assert_eq!(row.ppid, 60045);
        assert_eq!(row.age_secs, 514);
        assert!(row.args.ends_with("do pr wait 1483"));
    }

    const GRACE: i64 = 900;

    /// A spawn-origin, uncrowned row whose work is done everywhere it is
    /// named and whose transcript has been quiet past the grace: the AC3-HP
    /// base case.
    fn retiring() -> GcRow {
        GcRow {
            origin: Some("spawn".into()),
            crowned: false,
            work: WorkState::AllDone {
                nodes: vec!["N1".into()],
            },
            transcript_age_s: Some(GRACE + 1),
            owns_worktree: true,
            worktree_clean: Some(true),
            branch_merged: Some(true),
            planning: None,
            planning_closed: Vec::new(),
            planning_plan_written: Vec::new(),
            planning_released: false,
            turn_ended: false,
            confirm_hold: None,
            session_terminal: None,
            superseded_by_live_peer: None,
            node_merged: false,
            pid_gone: false,
            release_quiet: false,
            open_pr: None,
            peer_drives_pr: false,
            pr_settled: false,
            origin_corpse: false,
            open_work_retire_s: crate::agents_config::DEFAULT_OPEN_WORK_RETIRE_SECS as i64,
        }
    }

    /// The planning lane (AC3-HP): a blueprinter named on a node that reached
    /// ready has FINISHED its assignment - the plan was written, the node
    /// moved on, and THIS session's own blueprint row carries `ended_at`.
    /// Quiet past the 1200 s planner grace (d-81c6da7e), not the 900 s
    /// default, retires it without closing the feature or inventing a node.
    #[test]
    fn ac3_hp_planner_on_ready_node_completes_at_plan_written() {
        let planner = GcRow {
            work: WorkState::Open {
                node: "x-cccc".into(),
                status: "ready".into(),
            },
            planning: Some(vec![("x-cccc".to_string(), "ready".to_string())]),
            planning_closed: vec!["x-cccc".to_string()],
            transcript_age_s: Some(PLANNING_IDLE_RETIRE_SECS + 1),
            ..retiring()
        };
        assert_eq!(gc_decide(&planner, GRACE), (GcAction::Retire, None));
        // ...and stays eligible when its node is in flight: the planner is
        // not the implementer.
        let dispatched = GcRow {
            work: WorkState::Open {
                node: "x-cccc".into(),
                status: "in_progress".into(),
            },
            planning: Some(vec![("x-cccc".to_string(), "in_progress".to_string())]),
            planning_closed: vec!["x-cccc".to_string()],
            ..planner.clone()
        };
        assert_eq!(gc_decide(&dispatched, GRACE), (GcAction::Retire, None));
        // AC1-EDGE: the planner grace is 1200 s, not the 900 s default - at
        // 1100 s quiet the finished planner is still `active`.
        let young = GcRow {
            transcript_age_s: Some(1100),
            ..planner
        };
        assert_eq!(
            gc_decide(&young, GRACE),
            (GcAction::Keep, Some(KeepReason::Active { age_s: 1100 }))
        );
    }

    /// The planning lane's edge (AC3-EDGE, d-81c6da7e): one named node still
    /// at `idea` holds the planner as `PlanningUnclosed` - the plan it was
    /// dispatched to write never landed there, and the named reason is what
    /// the sweep's hold escalates on. A planning row with NO planning
    /// statuses (graph lost the join) still keeps under open work: an
    /// unjudgeable row is never retired on the planning lane.
    #[test]
    fn ac3_edge_planner_on_idea_node_stays_outstanding() {
        let planner = GcRow {
            work: WorkState::Open {
                node: "x-cccc".into(),
                status: "idea".into(),
            },
            planning: Some(vec![("x-cccc".to_string(), "idea".to_string())]),
            ..retiring()
        };
        assert_eq!(
            gc_decide(&planner, GRACE),
            (
                GcAction::Keep,
                Some(KeepReason::PlanningUnclosed {
                    node: "x-cccc".into(),
                    status: "idea".into(),
                })
            )
        );

        let unplannable = GcRow {
            work: WorkState::Open {
                node: "x-cccc".into(),
                status: "ready".into(),
            },
            planning: Some(Vec::new()),
            ..retiring()
        };
        assert_eq!(
            gc_decide(&unplannable, GRACE),
            (
                GcAction::Keep,
                Some(KeepReason::OpenWorkStale {
                    node: "x-cccc".into(),
                    status: "ready".into(),
                })
            )
        );
    }

    /// x-dddd AC4-HP: a `bp-` row whose node reads `ready` but which holds
    /// NEITHER finished marker (no `ended_at` of its own, no written plan)
    /// keeps its row - the completion belongs to an earlier assignment, and
    /// the reason names the unclosed node and its status. Fail closed: an
    /// absent closed set keeps the row too.
    #[test]
    fn ac4_hp_ready_node_without_a_closed_assignment_holds_the_row() {
        let replanner = GcRow {
            work: WorkState::Open {
                node: "x-dddd".into(),
                status: "ready".into(),
            },
            planning: Some(vec![("x-dddd".to_string(), "ready".to_string())]),
            planning_closed: Vec::new(),
            ..retiring()
        };
        assert_eq!(
            gc_decide(&replanner, GRACE),
            (
                GcAction::Keep,
                Some(KeepReason::PlanningUnclosed {
                    node: "x-dddd".into(),
                    status: "ready".into(),
                })
            )
        );
        // The fail-closed twin: a different node closed, this one not.
        let partial = GcRow {
            planning: Some(vec![
                ("x-dddd".to_string(), "ready".to_string()),
                ("x-9999".to_string(), "done".to_string()),
            ]),
            planning_closed: vec!["x-9999".to_string()],
            ..replanner
        };
        assert_eq!(
            gc_decide(&partial, GRACE),
            (
                GcAction::Keep,
                Some(KeepReason::PlanningUnclosed {
                    node: "x-dddd".into(),
                    status: "ready".into(),
                })
            )
        );
    }

    /// x-dddd AC4-EDGE / d-81c6da7e marker 1: the same row retires once its
    /// own blueprint row gains `ended_at` - past the 1200 s planner grace.
    #[test]
    fn ac4_edge_a_closed_assignment_releases_the_row() {
        let replanner = GcRow {
            work: WorkState::Open {
                node: "x-dddd".into(),
                status: "ready".into(),
            },
            planning: Some(vec![("x-dddd".to_string(), "ready".to_string())]),
            planning_closed: vec!["x-dddd".to_string()],
            transcript_age_s: Some(PLANNING_IDLE_RETIRE_SECS + 1),
            ..retiring()
        };
        assert_eq!(gc_decide(&replanner, GRACE), (GcAction::Retire, None));
    }

    /// d-81c6da7e marker 2 (AC1-HP, AC1-EDGE): a codex planner that wrote
    /// the node's plan - `plan_path` names an existing file, no earlier
    /// planner - has finished its assignment with no `ended_at` to close
    /// it. Twenty quiet minutes retire the row; nineteen keep it active.
    #[test]
    fn a_planner_that_wrote_the_plan_retires_after_twenty_quiet_minutes() {
        let planner = GcRow {
            work: WorkState::Open {
                node: "x-eeee".into(),
                status: "ready".into(),
            },
            planning: Some(vec![("x-eeee".to_string(), "ready".to_string())]),
            planning_plan_written: vec!["x-eeee".to_string()],
            transcript_age_s: Some(PLANNING_IDLE_RETIRE_SECS + 1),
            ..retiring()
        };
        assert_eq!(gc_decide(&planner, GRACE), (GcAction::Retire, None));
        let young = GcRow {
            transcript_age_s: Some(1100),
            ..planner
        };
        assert_eq!(
            gc_decide(&young, GRACE),
            (GcAction::Keep, Some(KeepReason::Active { age_s: 1100 }))
        );
    }

    /// d-81c6da7e AC3-EDGE: a released planner on an `idea` node retires
    /// past the planner grace - the release ruling answered the marker
    /// question, so only quiet remains.
    #[test]
    fn a_released_planner_on_an_idea_node_retires_past_the_planner_grace() {
        let planner = GcRow {
            work: WorkState::Open {
                node: "x-cccc".into(),
                status: "idea".into(),
            },
            planning: Some(vec![("x-cccc".to_string(), "idea".to_string())]),
            planning_released: true,
            transcript_age_s: Some(PLANNING_IDLE_RETIRE_SECS + 1),
            ..retiring()
        };
        assert_eq!(gc_decide(&planner, GRACE), (GcAction::Retire, None));
    }

    /// AC3-HP: a planner whose only assignment is a moved-on node -
    /// `deferred` or `superseded` - retires with no close and no plan:
    /// there is nothing left to plan.
    #[test]
    fn a_planner_on_a_moved_on_node_retires_without_a_marker() {
        for status in ["deferred", "superseded"] {
            let planner = GcRow {
                work: WorkState::Open {
                    node: "x-m1".into(),
                    status: status.into(),
                },
                planning: Some(vec![("x-m1".to_string(), status.to_string())]),
                transcript_age_s: Some(PLANNING_IDLE_RETIRE_SECS + 1),
                ..retiring()
            };
            assert_eq!(
                gc_decide(&planner, GRACE),
                (GcAction::Retire, None),
                "{status}"
            );
        }
    }

    /// AC3-ERR: a planner whose assignment is unfinished keeps as
    /// `PlanningUnclosed`. The sweep maps `blocked`, `working`, and an
    /// absent inside-leg report to the same fact: the turn has not ended.
    #[test]
    fn a_planner_whose_turn_has_not_ended_keeps() {
        let planner = GcRow {
            work: WorkState::Open {
                node: "x-m2".into(),
                status: "idea".into(),
            },
            planning: Some(vec![("x-m2".to_string(), "idea".to_string())]),
            turn_ended: false,
            transcript_age_s: Some(PLANNING_IDLE_RETIRE_SECS + 1),
            ..retiring()
        };
        assert_eq!(
            gc_decide(&planner, GRACE),
            (
                GcAction::Keep,
                Some(KeepReason::PlanningUnclosed {
                    node: "x-m2".into(),
                    status: "idea".into(),
                })
            )
        );
    }

    /// AC3-EDGE: a halted planner (latest inside-leg report `done`)
    /// on an unfinished node retires past the planner grace, and keeps as
    /// Active inside it.
    #[test]
    fn a_halted_planner_retires_past_the_grace_and_keeps_inside_it() {
        let halted = GcRow {
            work: WorkState::Open {
                node: "x-m3".into(),
                status: "idea".into(),
            },
            planning: Some(vec![("x-m3".to_string(), "idea".to_string())]),
            turn_ended: true,
            transcript_age_s: Some(PLANNING_IDLE_RETIRE_SECS + 1),
            ..retiring()
        };
        assert_eq!(gc_decide(&halted, GRACE), (GcAction::Retire, None));
        let young = GcRow {
            transcript_age_s: Some(1100),
            ..halted
        };
        assert_eq!(
            gc_decide(&young, GRACE),
            (GcAction::Keep, Some(KeepReason::Active { age_s: 1100 }))
        );
    }

    #[test]
    fn ac3_hp_work_done_and_quiet_retires_and_prunes() {
        let row = retiring();
        assert_eq!(gc_decide(&row, GRACE), (GcAction::Retire, None));
        assert_eq!(tree_action(&row), TreeAction::Prune);
    }

    #[test]
    fn stop_row_process_inside_an_ambient_block_on_does_not_panic() {
        // The CLI verb runs the sweep under main's block_on, so the stop seam
        // executes on a thread that already drives a runtime. The re-entrant
        // block_on that shape used to hit panicked the reap verb before it
        // classified a single row; the dedicated thread keeps it silent. A
        // default entry owns no socket, so nothing answers and nothing is
        // stopped - the subject is the absence of a panic, not the verdict.
        let dir = std::env::temp_dir().join(format!(
            "fno-gc-stop-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = AgentsHome::at(&dir);
        home.ensure_root().unwrap();
        let entry = crate::state::RegistryEntry::default();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _stopped = rt.block_on(async { gc_sweep::stop_row_process(&home, &entry) });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ac3_edge_claude_row_without_any_session_id_lands_in_stop_refused() {
        // A claude row owns no worker socket, so the old socket probe read
        // "down" instantly and the row dropped while the claude daemon still
        // held the session (the adopt-then-rm recovery, 50 times). Now a
        // claude row the stop cannot REACH - no short id, no session id -
        // answers false and lands in stop_refused: the sweep must not drop a
        // row whose sideline entry it cannot settle.
        let dir = std::env::temp_dir().join(format!(
            "fno-gc-claude-stop-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = AgentsHome::at(&dir);
        home.ensure_root().unwrap();
        let entry = crate::state::RegistryEntry {
            name: "target-x-ffff-worker".into(),
            cwd: dir.to_string_lossy().to_string(),
            harness: Some("claude".into()),
            ..Default::default()
        };
        assert!(!gc_sweep::stop_row_process(&home, &entry));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dry_run_never_stops_a_retiring_row() {
        // A rehearsal that killed the worker it rehearsed retiring would be
        // the destructive run wearing a dry flag. The stop seam must never
        // fire in dry-run, even for a row that classifies Retire end to end.
        use std::collections::HashMap;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let dir = std::env::temp_dir().join(format!(
            "fno-gc-dry-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = AgentsHome::at(&dir);
        home.ensure_root().unwrap();
        // A transcript quiet past grace: written, then aged 2000s.
        let transcript = dir.join("rollout.jsonl");
        std::fs::write(&transcript, b"{}\n").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(2000);
        std::fs::File::options()
            .write(true)
            .open(&transcript)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();
        crate::state::update_registry(&home.registry_json(), |r| {
            let mut e = crate::state::RegistryEntry::default();
            e.name = "dryw".into();
            e.short_id = "dryw".into();
            e.origin = Some("spawn".into());
            e.harness = Some("codex".into());
            e.harness_session_id = Some("S-dry".into());
            r.entries.push(e);
        })
        .unwrap();
        let mut index = HashMap::new();
        index.insert(
            "s-dry".to_string(),
            vec![("N1".to_string(), "done".to_string())],
        );
        let graph = std::cell::RefCell::new(Some(gc_sweep::GraphRead {
            work_index: index.clone(),
            index,
            open_do: HashMap::new(),
            phases: HashMap::new(),
            closed_planning: HashMap::new(),
            plan_written: HashMap::new(),
            statuses: HashMap::from([("N1".to_string(), "done".to_string())]),
            pr_state: HashMap::from([("N1".to_string(), (None, 0, 0))]),
            pr_number: HashMap::new(),
            do_nodes: HashMap::new(),
            pr_reads: HashMap::new(),
        }));
        let emitter = crate::events::EventEmitter::new(std::path::PathBuf::new(), "daemon");
        let stopped = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stopped);
        let summary = gc_sweep::run(
            &home,
            &emitter,
            900,
            true,
            7,
            &|_h| graph.borrow_mut().take(),
            &|_e| Some(vec![transcript.clone()]),
            &|entries| {
                entries
                    .iter()
                    .map(|e| (row_handle(e), Some(2 * 3600)))
                    .collect::<HashMap<_, _>>()
            },
            &move |_e| {
                flag.store(true, Ordering::SeqCst);
                true
            },
            &|_e| crate::daemon::CascadeOutcome::NotApplicable,
            &|_e| crate::daemon::CascadeOutcome::NotApplicable,
            &no_agents,
            &|_e| (None, None),
            &|_e| None,
        );
        assert!(
            !stopped.load(Ordering::SeqCst),
            "the stop seam fired in dry-run"
        );
        // the row still classifies would-retire, but a dry run with
        // no positive stop evidence holds it under needs_live_stop instead
        // of promising the retirement.
        assert_eq!(
            summary.needs_live_stop.len(),
            1,
            "the row still classifies would-retire: {summary:?}"
        );
        assert!(
            summary.retired.is_empty() && summary.dry_run_unverified.is_empty(),
            "a dry run promises nothing: {:?}",
            (summary.retired, summary.dry_run_unverified)
        );
        assert!(
            crate::state::load_registry(&home.registry_json())
                .unwrap()
                .entries
                .iter()
                .any(|e| e.name == "dryw"),
            "dry-run mutated the registry"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The per-candidate freshness re-check: activity arriving between
    /// classification and the stop effect keeps the row. The re-check re-stats
    /// THIS row's transcript through the same store seam; a fresh read, or a
    /// read that can no longer resolve, holds the retirement for the next
    /// tick.
    #[test]
    fn activity_arriving_in_the_apply_window_keeps_the_row() {
        use std::cell::Cell;
        use std::collections::HashMap;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let dir = std::env::temp_dir().join(format!(
            "fno-gc-fresh-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let home = AgentsHome::at(dir);
        home.ensure_root().unwrap();
        let transcript = home.root().join("rollout.jsonl");
        std::fs::write(&transcript, b"{}\n").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(2000);
        std::fs::File::options()
            .write(true)
            .open(&transcript)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();
        crate::state::update_registry(&home.registry_json(), |r| {
            let mut e = crate::state::RegistryEntry::default();
            e.name = "freshw".into();
            e.short_id = "freshw".into();
            e.origin = Some("spawn".into());
            e.harness = Some("codex".into());
            e.harness_session_id = Some("S-fresh".into());
            r.entries.push(e);
        })
        .unwrap();

        let mut index = HashMap::new();
        index.insert(
            "s-fresh".to_string(),
            vec![("N1".to_string(), "done".to_string())],
        );
        let graph = std::cell::RefCell::new(Some(gc_sweep::GraphRead {
            work_index: index.clone(),
            index,
            open_do: HashMap::new(),
            phases: HashMap::new(),
            closed_planning: HashMap::new(),
            plan_written: HashMap::new(),
            statuses: HashMap::from([("N1".to_string(), "done".to_string())]),
            pr_state: HashMap::from([("N1".to_string(), (None, 0, 0))]),
            pr_number: HashMap::new(),
            do_nodes: HashMap::new(),
            pr_reads: HashMap::new(),
        }));
        let emitter = crate::events::EventEmitter::new(std::path::PathBuf::new(), "daemon");
        let stopped = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stopped);
        let calls = Cell::new(0u32);
        let calls_ref = &calls;
        // Call 1 (classification): the age seam answers 2000s, past grace.
        // Call 2 (the apply-window re-check): the SAME row reads fresh, as if
        // the session just wrote a turn.
        let age_many = move |entries: &[&crate::state::RegistryEntry]| {
            let n = calls_ref.get();
            calls_ref.set(n + 1);
            let age = if n == 0 { 2000 } else { 0 };
            entries
                .iter()
                .map(|e| (row_handle(e), Some(age)))
                .collect::<HashMap<_, _>>()
        };
        let summary = gc_sweep::run(
            &home,
            &emitter,
            900, // grace
            false,
            7,
            &|_h| graph.borrow_mut().take(),
            &|_e| Some(vec![transcript.clone()]),
            &age_many,
            &move |_e| {
                flag.store(true, Ordering::SeqCst);
                true
            },
            &|_e| crate::daemon::CascadeOutcome::NotApplicable,
            &|_e| crate::daemon::CascadeOutcome::NotApplicable,
            &no_agents,
            &|_e| (None, None),
            &|_e| None,
        );
        assert!(
            !stopped.load(Ordering::SeqCst),
            "the stop fired despite fresh activity in the window"
        );
        assert!(
            summary.retired.is_empty(),
            "no retirement recorded: {:?}",
            summary.retired
        );
        assert_eq!(
            summary.kept_active.len(),
            1,
            "the row lands in kept_active: {:?}",
            summary.kept_active
        );
        let _ = std::fs::remove_dir_all(home.root());
    }

    #[test]
    fn a_non_spawn_row_names_its_own_gate_even_when_the_graph_is_unreadable() {
        // The origin gate runs BEFORE the graph read in the sweep, so a row
        // fno never spawned is held under not-a-spawn-row whatever the
        // graph's state - kept_graph_unreadable must not shadow the policy's
        // own first gate (gc_decide checks origin before work).
        let dir = std::env::temp_dir().join(format!(
            "fno-gc-notspawn-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = AgentsHome::at(&dir);
        home.ensure_root().unwrap();
        crate::state::update_registry(&home.registry_json(), |r| {
            let mut e = crate::state::RegistryEntry::default();
            e.name = "adop".into();
            e.short_id = "adop".into();
            e.origin = Some("adopted".into());
            e.harness = Some("codex".into());
            e.harness_session_id = Some("S-adop".into());
            r.entries.push(e);
        })
        .unwrap();
        let emitter = crate::events::EventEmitter::new(std::path::PathBuf::new(), "daemon");
        let summary = gc_sweep::run(
            &home,
            &emitter,
            900,
            false,
            7,
            // The graph read answers NOTHING: every row would class as
            // graph-unreadable if the origin gate did not run first.
            &|_| None,
            &|_| None,
            &|entries| {
                use std::collections::HashMap;
                entries
                    .iter()
                    .map(|e| (row_handle(e), Some(2 * 3600)))
                    .collect::<HashMap<_, _>>()
            },
            &|_| true,
            &|_| crate::daemon::CascadeOutcome::NotApplicable,
            &|_e| crate::daemon::CascadeOutcome::NotApplicable,
            &no_agents,
            &|_| (None, None),
            &|_| None,
        );
        assert_eq!(
            summary.kept_not_spawn,
            vec![("adop".to_string(), "adopted".to_string())]
        );
        assert!(
            summary.kept_graph_unreadable.is_empty(),
            "{:?}",
            summary.kept_graph_unreadable
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ac3_edge_operator_crown_open_work_and_active_keep() {
        let operator = GcRow {
            origin: Some("operator".into()),
            ..retiring()
        };
        assert_eq!(
            gc_decide(&operator, GRACE),
            (GcAction::Keep, Some(KeepReason::Operator))
        );

        let crowned = GcRow {
            crowned: true,
            ..retiring()
        };
        assert_eq!(
            gc_decide(&crowned, GRACE),
            (GcAction::Keep, Some(KeepReason::Crowned))
        );

        // Only a spawn row retires: adopted (and an unrecorded origin) keep
        // whatever the work state says - done-plus-quiet is not fno's to act
        // on for a session it did not spawn.
        for origin in ["adopted", "operator", ""] {
            let not_spawn = GcRow {
                origin: if origin.is_empty() {
                    None
                } else {
                    Some(origin.into())
                },
                ..retiring()
            };
            let (action, reason) = gc_decide(&not_spawn, GRACE);
            assert_eq!(action, GcAction::Keep, "origin {origin:?}");
            assert!(
                matches!(
                    reason,
                    Some(KeepReason::NotSpawn { .. }) | Some(KeepReason::Operator)
                ),
                "origin {origin:?} named its gate"
            );
        }

        let open = GcRow {
            work: WorkState::Open {
                node: "N3".into(),
                status: "in_review".into(),
            },
            ..retiring()
        };
        assert_eq!(
            gc_decide(&open, GRACE),
            (
                GcAction::Keep,
                Some(KeepReason::OpenWorkStale {
                    node: "N3".into(),
                    status: "in_review".into()
                })
            )
        );
        // change 2: an open row with NO transcript age has no clock,
        // so it keeps under the unchanged open-work reason - it can never
        // age past the window, and absence is never quiet.
        let unclocked = GcRow {
            work: WorkState::Open {
                node: "N3".into(),
                status: "in_review".into(),
            },
            transcript_age_s: None,
            ..retiring()
        };
        assert_eq!(
            gc_decide(&unclocked, GRACE),
            (
                GcAction::Keep,
                Some(KeepReason::OpenWork {
                    node: "N3".into(),
                    status: "in_review".into()
                })
            )
        );

        // A transcript written 10 seconds ago: the session is writing, keep.
        let active = GcRow {
            transcript_age_s: Some(10),
            ..retiring()
        };
        assert_eq!(
            gc_decide(&active, GRACE),
            (GcAction::Keep, Some(KeepReason::Active { age_s: 10 }))
        );
        // Exactly at the grace boundary is still inside it.
        let boundary = GcRow {
            transcript_age_s: Some(GRACE),
            ..retiring()
        };
        assert_eq!(
            gc_decide(&boundary, GRACE),
            (GcAction::Keep, Some(KeepReason::Active { age_s: GRACE }))
        );
    }

    #[test]
    fn ac3_err_unresolved_transcript_no_provenance_and_tree_buckets() {
        let unresolved = GcRow {
            transcript_age_s: None,
            ..retiring()
        };
        assert_eq!(
            gc_decide(&unresolved, GRACE),
            (GcAction::Keep, Some(KeepReason::TranscriptUnresolved))
        );

        let no_provenance = GcRow {
            work: WorkState::NoProvenance,
            ..retiring()
        };
        assert_eq!(
            gc_decide(&no_provenance, GRACE),
            (GcAction::Keep, Some(KeepReason::NoProvenance))
        );

        // The tree buckets, on rows that DID retire: a dirty or unmerged or
        // unprobed tree never blocks the row, it only keeps the tree.
        let dirty = GcRow {
            worktree_clean: Some(false),
            ..retiring()
        };
        assert_eq!(gc_decide(&dirty, GRACE), (GcAction::Retire, None));
        assert_eq!(tree_action(&dirty), TreeAction::KeepDirty);

        let unmerged = GcRow {
            branch_merged: Some(false),
            ..retiring()
        };
        assert_eq!(gc_decide(&unmerged, GRACE), (GcAction::Retire, None));
        assert_eq!(tree_action(&unmerged), TreeAction::KeepUnmerged);

        let unprobed = GcRow {
            worktree_clean: None,
            ..retiring()
        };
        assert_eq!(gc_decide(&unprobed, GRACE), (GcAction::Retire, None));
        assert_eq!(tree_action(&unprobed), TreeAction::KeepUnprobed);

        let detached = GcRow {
            branch_merged: None,
            ..retiring()
        };
        assert_eq!(tree_action(&detached), TreeAction::KeepUnmerged);

        // A row owning nothing removable: no tree question at all.
        let bare = GcRow {
            owns_worktree: false,
            ..retiring()
        };
        assert_eq!(tree_action(&bare), TreeAction::None);

        // origin None is NOT the spawn fact: with no origin recorded, done
        // work and a quiet transcript still keep the row - only an explicit
        // spawn origin retires, and retiring() above carries one.
        let unstamped = GcRow {
            origin: None,
            ..retiring()
        };
        assert_eq!(
            gc_decide(&unstamped, GRACE),
            (
                GcAction::Keep,
                Some(KeepReason::NotSpawn { origin: "".into() })
            )
        );
    }

    #[test]
    fn keep_reason_tags_name_their_gate() {
        assert_eq!(KeepReason::Operator.as_str(), "operator");
        assert_eq!(KeepReason::Crowned.as_str(), "crowned");
        assert_eq!(
            KeepReason::NoProvenance.as_str(),
            "no provenance: no source resolved a node (sessions, registry, name, transcript)"
        );
        assert_eq!(
            KeepReason::NodeConflict {
                a: "x-aaaa".into(),
                b: "x-bbbb".into()
            }
            .as_str(),
            "sources disagree"
        );
        assert_eq!(
            KeepReason::PrStateContradicts {
                node: "x-aaaa".into(),
                detail: "additional_prs: 1".into()
            }
            .as_str(),
            "pr state contradicts"
        );
        assert_eq!(
            KeepReason::OpenWork {
                node: "N".into(),
                status: "ready".into()
            }
            .as_str(),
            "open work"
        );
        assert_eq!(KeepReason::Active { age_s: 5 }.as_str(), "active");
        assert_eq!(
            KeepReason::TranscriptUnresolved.as_str(),
            "transcript unresolved"
        );
        assert_eq!(KeepReason::GraphUnreadable.as_str(), "graph unreadable");
        assert_eq!(
            KeepReason::OpenDoRow { node: "N".into() }.as_str(),
            "open do row on done node"
        );
    }

    // ──: the reaper asks the session, not only the node ──────────

    /// An open-work row that is quiet past the grace - the shape the old
    /// policy held forever.
    fn open_row(status: &str) -> GcRow {
        GcRow {
            work: WorkState::Open {
                node: "N1".into(),
                status: status.into(),
            },
            ..retiring()
        }
    }

    /// Change 1: the harness publishing a terminal state overrides the
    /// open-work keep. The grace gate still rules on the OTHER guards: a
    /// non-terminal state keeps under open work, and a terminal state with
    /// an unresolved transcript keeps under transcript unresolved.
    #[test]
    fn terminal_session_state_releases_the_open_work_keep() {
        let mut row = open_row("in_review");
        row.session_terminal = Some("done".into());
        assert_eq!(gc_decide(&row, GRACE), (GcAction::Retire, None));

        row.transcript_age_s = Some(10);
        assert_eq!(
            gc_decide(&row, GRACE),
            (GcAction::Retire, None),
            "the roster measurement: `done` is a finish line, not a turn \
             boundary - a between-turns session reads working/idle, never done"
        );
        // A non-terminal state never reaches this field: the population
        // site filters through is_terminal_roster_state, covered at sweep
        // level by x2774_terminal_harness_state_releases_an_open_work_row.
    }

    /// recency yields to a terminal harness state. An AllDone row
    /// inside the grace window retires when its roster state reads done and
    /// names the early fire when `working` or `blocked` - not terminal -
    /// keeps it under active, exactly as today.
    #[test]
    fn a_terminal_harness_state_overrides_recency() {
        let mut retiring_row = retiring();
        retiring_row.transcript_age_s = Some(274);
        retiring_row.session_terminal = Some("done".into());
        assert_eq!(gc_decide(&retiring_row, GRACE), (GcAction::Retire, None));

        let mut working = retiring();
        working.transcript_age_s = Some(274);
        working.session_terminal = None;
        assert_eq!(
            gc_decide(&working, GRACE),
            (GcAction::Keep, Some(KeepReason::Active { age_s: 274 })),
            "no terminal fact, no override"
        );

        let mut unresolved = retiring();
        unresolved.transcript_age_s = None;
        unresolved.session_terminal = Some("done".into());
        assert_eq!(
            gc_decide(&unresolved, GRACE),
            (GcAction::Keep, Some(KeepReason::TranscriptUnresolved),),
            "a terminal state never makes an unreadable transcript quiet"
        );
    }

    /// Change 3: a live newer peer on the same node releases the shield.
    #[test]
    fn a_live_newer_peer_releases_the_open_work_keep() {
        let mut row = open_row("in_review");
        row.superseded_by_live_peer = Some("newer (created 2026-09-09T23:00:00Z)".into());
        assert_eq!(gc_decide(&row, GRACE), (GcAction::Retire, None));
    }

    /// Change 6: a parked or never-started node is not evidence a session
    /// is alive. `in_review` still holds a non-planner row.
    #[test]
    fn an_inactive_node_status_releases_the_open_work_keep() {
        for status in ["deferred", "idea"] {
            let row = open_row(status);
            assert_eq!(
                gc_decide(&row, GRACE),
                (GcAction::Retire, None),
                "status {status} is not active work"
            );
        }
        assert!(matches!(
            gc_decide(&open_row("in_review"), GRACE),
            (GcAction::Keep, Some(KeepReason::OpenWorkStale { .. }))
        ));
    }

    /// Change 6: a recorded merge the node status lags is not active work.
    #[test]
    fn a_recorded_merge_releases_the_open_work_keep() {
        let mut row = open_row("in_progress");
        row.node_merged = true;
        assert_eq!(gc_decide(&row, GRACE), (GcAction::Retire, None));
    }

    /// A settled PR (GitHub answered merged or closed) is a fifth positive
    /// fact: the open-PR keep has nothing to hold on, so the row falls to
    /// the grace gate - retiring quiet, staying active fresh.
    #[test]
    fn a_settled_pr_releases_the_row_through_the_grace_gate() {
        let mut row = open_row("in_review");
        row.pr_settled = true;
        assert_eq!(gc_decide(&row, GRACE), (GcAction::Retire, None));
        row.transcript_age_s = Some(10);
        assert_eq!(
            gc_decide(&row, GRACE),
            (GcAction::Keep, Some(KeepReason::Active { age_s: 10 })),
            "a settled PR never overrides recency: fresh is fresh"
        );
    }

    /// An adopted row that is provably a corpse is judged like any other:
    /// the origin gate skips it, and a done-and-quiet adopted row retires
    /// through the same gates a spawn row takes. A live adopted row keeps
    /// under the unchanged not-a-spawn reason.
    #[test]
    fn an_adopted_corpse_is_judged_like_any_other_row() {
        let mut corpse = retiring();
        corpse.origin = Some("adopted".into());
        corpse.origin_corpse = true;
        assert_eq!(gc_decide(&corpse, GRACE), (GcAction::Retire, None));
        let mut live = retiring();
        live.origin = Some("adopted".into());
        assert!(matches!(
            gc_decide(&live, GRACE),
            (GcAction::Keep, Some(KeepReason::NotSpawn { .. }))
        ));
    }

    /// A row that resolved no node releases on its own done report: the
    /// latest inside-leg leg reads `done` and fno never stopped the row,
    /// so the grace gate supplies the quiet conjunct. A row still working,
    /// or one fno stopped, keeps under no provenance.
    #[test]
    fn a_no_provenance_row_releases_on_its_own_done_report() {
        let base = GcRow {
            work: WorkState::NoProvenance,
            ..retiring()
        };
        let mut done = base.clone();
        done.turn_ended = true;
        assert_eq!(gc_decide(&done, GRACE), (GcAction::Retire, None));
        done.transcript_age_s = Some(10);
        assert_eq!(
            gc_decide(&done, GRACE),
            (GcAction::Keep, Some(KeepReason::Active { age_s: 10 })),
            "the done report releases the forever keep, never recency"
        );
        assert_eq!(
            gc_decide(&base, GRACE),
            (GcAction::Keep, Some(KeepReason::NoProvenance)),
            "a row that never reported done keeps"
        );
        let mut stopped = base.clone();
        stopped.turn_ended = false;
        assert_eq!(
            gc_decide(&stopped, GRACE),
            (GcAction::Keep, Some(KeepReason::NoProvenance)),
            "a row fno stopped keeps: turn_ended is false for it"
        );
    }

    /// Change 8: a provably dead pid (ESRCH) overrides transcript recency,
    /// but never transcript UNRESOLVED - absence is not quiet even for a
    /// dead pid, because a dead pid says nothing about the transcript.
    #[test]
    fn a_dead_pid_overrides_recency_but_not_unresolved() {
        let mut row = retiring();
        row.transcript_age_s = Some(100);
        assert_eq!(
            gc_decide(&row, GRACE),
            (GcAction::Keep, Some(KeepReason::Active { age_s: 100 })),
        );
        row.pid_gone = true;
        assert_eq!(gc_decide(&row, GRACE), (GcAction::Retire, None));

        let mut unresolved = retiring();
        unresolved.transcript_age_s = None;
        unresolved.pid_gone = true;
        assert_eq!(
            gc_decide(&unresolved, GRACE),
            (GcAction::Keep, Some(KeepReason::TranscriptUnresolved),),
            "dead pid does not make an unreadable transcript quiet"
        );
    }

    /// Change 1, inverse: no terminal state, live transcript - the row
    /// keeps. The release is never a blanket sweep.
    #[test]
    fn a_live_session_on_an_open_node_keeps_its_row() {
        let row = open_row("in_review");
        assert!(matches!(
            gc_decide(&row, GRACE),
            (GcAction::Keep, Some(KeepReason::OpenWorkStale { .. }))
        ));
    }

    /// change 2, AC2-EDGE: an Open row quiet PAST the open-work
    /// window falls to the grace gate and retires; the same row INSIDE the
    /// window keeps, and the keep names its pinning node.
    #[test]
    fn an_open_row_past_the_open_work_window_retires() {
        let row = GcRow {
            transcript_age_s: Some(crate::agents_config::DEFAULT_OPEN_WORK_RETIRE_SECS as i64 + 1),
            ..open_row("in_progress")
        };
        assert_eq!(gc_decide(&row, GRACE), (GcAction::Retire, None));
        let inside = GcRow {
            transcript_age_s: Some(crate::agents_config::DEFAULT_OPEN_WORK_RETIRE_SECS as i64 - 1),
            ..open_row("in_progress")
        };
        assert_eq!(
            gc_decide(&inside, GRACE),
            (
                GcAction::Keep,
                Some(KeepReason::OpenWorkStale {
                    node: "N1".into(),
                    status: "in_progress".into()
                })
            )
        );
    }

    /// change 2, AC2-HP: a NoProvenance row whose SESSION carries a
    /// terminal state but which never reported a finished turn reaches the
    /// grace gate - a quiet row past the grace retires instead of keeping
    /// forever on the missing turn marker.
    #[test]
    fn a_terminal_session_releases_a_no_provenance_row_without_turn_ended() {
        let row = GcRow {
            work: WorkState::NoProvenance,
            session_terminal: Some("done".into()),
            ..retiring()
        };
        assert_eq!(gc_decide(&row, GRACE), (GcAction::Retire, None));
        // The release still lands in the grace gate: the turn-ended release
        // keeps its quiet conjunct, so a row inside the grace keeps as
        // active (the terminal override, not this widened arm, is what
        // retires a young terminal row).
        let young = GcRow {
            work: WorkState::NoProvenance,
            turn_ended: true,
            transcript_age_s: Some(10),
            ..retiring()
        };
        assert_eq!(
            gc_decide(&young, GRACE),
            (GcAction::Keep, Some(KeepReason::Active { age_s: 10 }))
        );
    }

    /// change 1: the handle a row is probed under prefers the
    /// harness session id - `fno agents truth` resolves a full session id
    /// for registry AND roster rows alike, while a short id resolves for
    /// registry rows only. A registry row with no session id falls back to
    /// its short id, so the registry sweep keeps answering as it does today
    /// (AC1-EDGE).
    #[test]
    fn row_handle_prefers_the_session_id_and_falls_back_to_short_id() {
        let roster_only = crate::state::RegistryEntry::new(
            Some("11111111-2222-4333-8444-555555555555".to_string()),
            crate::state::Lineage::unproven("test"),
        );
        assert_eq!(
            crate::gc::row_handle(&roster_only),
            "11111111-2222-4333-8444-555555555555"
        );
        let mut e = crate::state::RegistryEntry::default();
        e.harness_session_id = None;
        e.short_id = "ab12cd34".into();
        assert_eq!(crate::gc::row_handle(&e), "ab12cd34");
        e.short_id = String::new();
        e.name = "worker-1".into();
        assert_eq!(crate::gc::row_handle(&e), "worker-1");
    }

    /// An adopted orphan row named on no node survives the sweep:
    /// NoProvenance -> Keep, addressable until the operator resumes it or a
    /// node names it. Moved here from client_verbs.rs, which is over the
    /// file budget and may only shrink; the policy is this module's.
    #[test]
    fn gc_keeps_synthesized_idle_row() {
        let row = GcRow {
            origin: Some("spawn".into()),
            crowned: false,
            work: WorkState::NoProvenance,
            transcript_age_s: Some(10_000),
            owns_worktree: true,
            worktree_clean: None,
            branch_merged: None,
            planning: None,
            planning_closed: Vec::new(),
            planning_plan_written: Vec::new(),
            planning_released: false,
            turn_ended: false,
            confirm_hold: None,
            session_terminal: None,
            superseded_by_live_peer: None,
            node_merged: false,
            pid_gone: false,
            release_quiet: false,
            open_pr: None,
            peer_drives_pr: false,
            pr_settled: false,
            origin_corpse: false,
            open_work_retire_s: crate::agents_config::DEFAULT_OPEN_WORK_RETIRE_SECS as i64,
        };
        assert_eq!(gc_decide(&row, 60).0, GcAction::Keep);
    }

    /// The base open-PR row: a do-phase spawn row on an in_review node
    /// whose PR is unmerged. `pr` and `node` parameterize the arms below.
    fn open_pr_row(node: &str, status: &str, pr: u64) -> GcRow {
        GcRow {
            origin: Some("spawn".into()),
            crowned: false,
            work: WorkState::Open {
                node: node.into(),
                status: status.into(),
            },
            transcript_age_s: Some(10_000),
            owns_worktree: true,
            worktree_clean: None,
            branch_merged: None,
            planning: None,
            planning_closed: Vec::new(),
            planning_plan_written: Vec::new(),
            planning_released: false,
            turn_ended: false,
            confirm_hold: None,
            session_terminal: None,
            superseded_by_live_peer: None,
            node_merged: false,
            pid_gone: false,
            release_quiet: false,
            open_pr: Some((node.into(), pr)),
            peer_drives_pr: false,
            pr_settled: false,
            origin_corpse: false,
            open_work_retire_s: crate::agents_config::DEFAULT_OPEN_WORK_RETIRE_SECS as i64,
        }
    }

    /// The keep outranks a terminal harness state: a stopped roster state
    /// on an in_review node with an open PR must not release the row, or
    /// the PR strands with nothing left to drive it.
    #[test]
    fn open_pr_keep_holds_a_terminal_session_without_a_pr_driver() {
        let mut row = open_pr_row("x-node", "in_review", 1943);
        row.session_terminal = Some("stopped".into());
        row.transcript_age_s = Some(40);
        let (action, reason) = gc_decide(&row, 900);
        assert_eq!(action, GcAction::Keep);
        assert_eq!(
            reason,
            Some(KeepReason::OpenPr {
                node: "x-node".into(),
                pr: 1943,
            })
        );
    }

    /// Every other release still answers to the keep: a live newer peer
    /// that does NOT drive the PR, and a parked node.
    #[test]
    fn open_pr_keep_survives_a_non_driving_peer_and_a_parked_node() {
        let mut row = open_pr_row("x-node", "in_review", 1943);
        row.session_terminal = Some("stopped".into());
        row.superseded_by_live_peer = Some("peer-b (created later)".into());
        let (action, reason) = gc_decide(&row, 900);
        assert_eq!(action, GcAction::Keep);
        assert_eq!(
            reason,
            Some(KeepReason::OpenPr {
                node: "x-node".into(),
                pr: 1943,
            })
        );
        let parked = open_pr_row("x-node", "deferred", 1943);
        assert_eq!(
            gc_decide(&parked, 900),
            (
                GcAction::Keep,
                Some(KeepReason::OpenPr {
                    node: "x-node".into(),
                    pr: 1943,
                })
            )
        );
    }

    /// The peer releases only when the PEER drives the PR: with a driving
    /// peer the row falls through to the ordinary release path and retires
    /// past the grace. A recorded merge empties open_pr, so the merged node
    /// retires as today too.
    #[test]
    fn a_driving_peer_and_a_recorded_merge_release_the_row() {
        let mut driven = open_pr_row("x-node", "in_review", 1943);
        driven.peer_drives_pr = true;
        driven.superseded_by_live_peer = Some("peer-b (created later)".into());
        assert_eq!(gc_decide(&driven, 900), (GcAction::Retire, None));
        let mut merged = open_pr_row("x-node", "in_review", 1943);
        merged.node_merged = true;
        merged.open_pr = None;
        assert_eq!(gc_decide(&merged, 900), (GcAction::Retire, None));
    }

    /// The planning lane keeps precedence: a blueprint row that closed its
    /// own planning assignment on an open-PR node retires past the grace -
    /// the planner never drives the feature PR.
    #[test]
    fn a_closed_planner_row_on_an_open_pr_node_retires_as_today() {
        let mut planner = open_pr_row("x-node", "in_review", 1943);
        planner.planning = Some(vec![("x-node".into(), "in_review".into())]);
        planner.planning_closed = vec!["x-node".into()];
        assert_eq!(gc_decide(&planner, 900), (GcAction::Retire, None));
    }
}
