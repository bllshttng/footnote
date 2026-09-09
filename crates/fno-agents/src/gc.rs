//! Row retirement, keyed by the reverse join through `node.sessions[]`
//! (x-c672).
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
    /// (x-5aef task 1.2). Empty on any other row. An empty set never
    /// retires a planner: an assignment nobody closed stays outstanding.
    pub planning_closed: Vec<String>,
    /// A hold computed beside the work verdict (x-5a62): the cascade's
    /// conflict between witnesses, or the PR-state confirm contradicting a
    /// done node. Decided in the sweep where the route and the graph read
    /// live; relayed here so the keep is named by the policy, never silently
    /// dropped. `None` when nothing holds.
    pub confirm_hold: Option<KeepReason>,
}

/// The statuses that complete a PLANNING assignment: the plan was written
/// and the node moved on (dispatched, in flight, or shipped). `idea` is the
/// loud exception - an idea node never received the plan, so the planning
/// assignment it was meant for is not finished (AC3-EDGE: an uncompleted
/// revision assignment stays outstanding).
pub const PLANNING_COMPLETE_STATUSES: [&str; 5] =
    ["done", "ready", "in_progress", "in_review", "shipped"];

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
    /// No declared source resolved a node for the session (x-5a62): the
    /// reverse join, the registry field, the row name, and the transcript
    /// all answered nothing (d-bbcd48b5 recovers provenance from any
    /// declared source; nothing left to recover from is the one honest
    /// keep).
    NoProvenance,
    /// Two provenance sources resolved DIFFERENT nodes (x-5a62): witnesses
    /// that disagree are not evidence, so the row is held rather than
    /// retired on a guess.
    NodeConflict { a: String, b: String },
    /// The node reads done but its PR state contradicts (x-5a62): an open
    /// additional PR, or a RECORDED merge_status that is not `merged`. An
    /// absent merge_status does not hold - absence has three explanations
    /// and none is `unmerged` - and rides the basis as unrecorded instead.
    PrStateContradicts { node: String, detail: String },
    /// The node reads planning-complete, but THIS session's own
    /// blueprint/think row on it carries no `ended_at` (x-5aef): the
    /// completion belongs to an earlier assignment, so this quiet
    /// replanning worker keeps its row with the unclosed node named.
    PlanningUnclosed { node: String },
    /// At least one named node is not done; the first open one is reported.
    OpenWork { node: String, status: String },
    /// The transcript was written inside the grace window: the session is
    /// live in the only sense the law allows.
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
                "planning assignment never closed by this session"
            }
            KeepReason::OpenWork { .. } => "open work",
            KeepReason::Active { .. } => "active",
            KeepReason::TranscriptUnresolved => "transcript unresolved",
            KeepReason::GraphUnreadable => "graph unreadable",
            KeepReason::OpenDoRow { .. } => "open do row on done node",
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
    // remove.
    if row.origin.as_deref() != Some("spawn") {
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
        WorkState::NoProvenance => (GcAction::Keep, Some(KeepReason::NoProvenance)),
        WorkState::Open { node, status } => {
            // The planning lane: the ROW's own job (write the plan) ends at
            // node-ready, so a planner whose every named node has moved past
            // planning is done with its work even though the feature is not
            // shipped (AC3-HP). One open node still parked at `idea` (or any
            // non-complete status) holds the row: the plan it was dispatched
            // to write never landed there.
            // x-5aef task 1.2 binds that verdict to the CURRENT assignment:
            // every node must ALSO sit in the set this session closed (its
            // own blueprint/think row carrying `ended_at`). A quiet
            // replanning worker inherits no completion an earlier blueprint
            // wrote. An absent or empty closed set fails closed, exactly as
            // the empty-status guard below does.
            if let Some(assignments) = &row.planning {
                // An EMPTY status set fails closed: a lane that fires on a
                // vacuous all() would retire a row the graph could not
                // describe.
                if !assignments.is_empty()
                    && assignments
                        .iter()
                        .all(|(_, s)| PLANNING_COMPLETE_STATUSES.contains(&s.as_str()))
                {
                    let unclosed = assignments
                        .iter()
                        .find(|(n, _)| !row.planning_closed.contains(n));
                    return match unclosed {
                        None => grace_gate(row, grace_secs),
                        Some((n, _)) => (
                            GcAction::Keep,
                            Some(KeepReason::PlanningUnclosed { node: n.clone() }),
                        ),
                    };
                }
            }
            (
                GcAction::Keep,
                Some(KeepReason::OpenWork {
                    node: node.clone(),
                    status: status.clone(),
                }),
            )
        }
        WorkState::AllDone { .. } => grace_gate(row, grace_secs),
    }
}

/// The transcript gates shared by every retire-eligible arm: an unresolved
/// transcript and a transcript inside the grace window both keep the row.
fn grace_gate(row: &GcRow, grace_secs: i64) -> (GcAction, Option<KeepReason>) {
    match row.transcript_age_s {
        None => (GcAction::Keep, Some(KeepReason::TranscriptUnresolved)),
        Some(age) if age <= grace_secs => (GcAction::Keep, Some(KeepReason::Active { age_s: age })),
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
pub(crate) fn row_handle(e: &crate::state::RegistryEntry) -> String {
    if e.short_id.is_empty() {
        e.name.clone()
    } else {
        e.short_id.clone()
    }
}

/// Seconds since the newest of the store's matches was written. Newest, not
/// first: a session can leave stubs in other project dirs, and a stub whose
/// creation post-dates the real transcript's last turn must not read as
/// fresher than it is. `None` when no match resolves: an unresolved
/// transcript is never a quiet one.
pub fn transcript_age_s(store_hits: Option<&[std::path::PathBuf]>, now: i64) -> Option<i64> {
    let newest = store_hits?.iter().max_by_key(|p| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0)
    })?;
    let mtime = std::fs::metadata(newest)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)?;
    Some(now.saturating_sub(mtime))
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
        &|e| gc_sweep::stop_row_process(home, e),
        &crate::gc_native::apply_active_surface_removal,
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

/// `fno agents reap --dry-run`: classify exactly as [`gc_sweep`] does, name
/// every row under exactly one bucket, mutate nothing - a reaper an operator
/// cannot rehearse is one they will not run.
pub fn gc_sweep_dry_run(home: &AgentsHome, grace_secs: i64) -> gc_sweep::GcSummary {
    // The settle plan is read-only, and the rehearsal subtracts it from the
    // graph read so the report shows the outcome the real pass would produce.
    let planned = gc_sweep::plan_stale_do_rows(home);
    let read = |h: &AgentsHome| {
        gc_sweep::read_graph_entries(h).map(|g| gc_sweep::without_settled(g, &planned))
    };
    // Never emitted to in dry-run mode (the whole write+emit tail is skipped),
    // so an unused placeholder path satisfies the shared signature.
    let emitter = EventEmitter::new(std::path::PathBuf::new(), "daemon");
    let store = std::cell::RefCell::new(gc_sweep::HarnessStoreIndex::default());
    let mut summary = gc_sweep::run(
        home,
        &emitter,
        grace_secs,
        true,
        0, // dry-run never expires: a rehearsal that pruned would not be one
        &read,
        &|e| store.borrow_mut().matches(e),
        &|e| gc_sweep::stop_row_process(home, e),
        &crate::gc_native::apply_active_surface_removal,
        &crate::claude_roster::read_all_agents,
        &gc_sweep::production_tree_probe,
        &crate::daemon::rm_take_worktree,
    );
    summary.settled_do_rows = planned
        .into_iter()
        .map(|row| (row.node, row.harness, row.session_id))
        .collect();
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

fn state_reap_family_counts(family: &gc_sweep::StateReapFamilySummary) -> serde_json::Value {
    serde_json::json!({
        "scanned": family.scanned,
        "deleted": family.deleted,
        "bytes": family.bytes,
        "kept": family.kept.len(),
    })
}

/// Apply the configured expendable-state retention policy and record one
/// bounded outcome event, including quiet and disabled passes.
pub fn state_file_sweep(
    home: &AgentsHome,
    emitter: &EventEmitter,
    cwd: &std::path::Path,
) -> gc_sweep::StateFilesReapSummary {
    let summary =
        gc_sweep::reap_state_files(home, crate::agents_config::state_reap_config(cwd), true);
    let _ = emitter.emit(
        "state_reap",
        &serde_json::json!({
            "expired_claims": state_reap_family_counts(&summary.expired_claims),
            "plan_locks": state_reap_family_counts(&summary.plan_locks),
            "agent_locks": state_reap_family_counts(&summary.agent_locks),
            "pr_status_cache": state_reap_family_counts(&summary.pr_status_cache),
            "totals": {
                "scanned": summary.totals.scanned,
                "deleted": summary.totals.deleted,
                "would_delete": summary.totals.would_delete,
                "bytes": summary.totals.bytes,
                "kept": summary.totals.kept,
            },
            "applied": summary.applied,
            "dry_run": summary.dry_run,
            "skip_reason": summary.skip_reason,
        }),
    );
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
/// (x-d354). Before this guard the sweep was requested every 5s tick against
/// a 900s grace, its settle pass writing the graph before it knew whether
/// there was any work: a continuous graph consumer wearing a cadence label,
/// with the one-in-flight gate ensuring the copies never stacked but never
/// slowed either. Shaped like [`crate::orphan_reap::maybe_sweep`]: elapsed
/// check, one-in-flight swap, stamp, off-loop body. The emitted `retire`
/// tick row carries the same `interval` the guard compared, so the arms
/// readout and the loop read one number.
pub fn maybe_retirement_sweep(
    last_sweep: &mut Instant,
    in_flight: &Arc<AtomicBool>,
    next_interval: &Arc<RetireIntervalCell>,
    home: AgentsHome,
    grace_cwd: PathBuf,
    events: PathBuf,
    interval: Duration,
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
        let summary = gc_sweep(&home, &emitter, grace_secs, retain_days);
        unowned_sweeps(&home, &emitter, &grace_cwd);
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
            summary.retired.len() as u64,
            None,
            None,
            interval.as_secs(),
        );
    });
}

#[cfg(test)]
mod tests {
    fn no_agents() -> crate::claude_roster::ClaudeAgentsSnapshot {
        crate::claude_roster::ClaudeAgentsSnapshot::unknown("test: no snapshot staged")
    }

    use super::*;

    // --- the retirement sweep arm (x-d354) ---

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
        (dir, home)
    }

    fn count_retire_rows(path: &std::path::Path) -> usize {
        std::fs::read_to_string(path)
            .map(|content| {
                content
                    .lines()
                    .filter(|l| {
                        l.contains("\"type\":\"control_plane_tick\"")
                            && l.contains("\"arm\":\"retire\"")
                    })
                    .count()
            })
            .unwrap_or(0)
    }

    fn wait_for_retire_row(path: &std::path::Path) -> usize {
        let deadline = Instant::now() + Duration::from_secs(5);
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
            );
            wait_for_retire_row(&home.events_jsonl());
            let row = std::fs::read_to_string(home.events_jsonl())
                .unwrap()
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

    #[test]
    fn state_reap_event_reports_counts() {
        let root = tempfile::tempdir().unwrap();
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
        let claim = root.path().join("claims/.expired/old-claim");
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
        let lines: Vec<serde_json::Value> = std::fs::read_to_string(home.events_jsonl())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2, "each periodic pass must emit one event");
        let event = &lines[0];
        assert_eq!(event["type"], "state_reap");
        assert_eq!(event["source"], "test");
        let data = &event["data"];
        for family in [
            "expired_claims",
            "plan_locks",
            "agent_locks",
            "pr_status_cache",
        ] {
            assert!(data[family].is_object(), "missing {family}: {data}");
            for count in ["scanned", "deleted", "bytes", "kept"] {
                assert!(
                    data[family][count].is_number(),
                    "missing {family}.{count}: {data}"
                );
            }
        }
        assert_eq!(data["expired_claims"]["deleted"], 1);
        assert_eq!(data["totals"]["deleted"], 1);
        assert_eq!(data["applied"], true);
        assert_eq!(data["dry_run"], false);
        assert!(data["skip_reason"].is_null());
        assert_eq!(lines[1]["type"], "state_reap");
        assert_eq!(lines[1]["data"]["totals"]["scanned"], 0);
        assert_eq!(lines[1]["data"]["totals"]["deleted"], 0);
        assert!(lines[1]["data"]["skip_reason"].is_null());
    }

    #[test]
    fn state_reap_event_reports_disabled_pass() {
        let root = tempfile::tempdir().unwrap();
        let home = AgentsHome::at(root.path().join("agents"));
        home.ensure_root().unwrap();
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
        let raw = std::fs::read_to_string(home.events_jsonl()).unwrap();
        let event: serde_json::Value = serde_json::from_str(raw.trim()).unwrap();
        assert_eq!(event["type"], "state_reap");
        assert_eq!(event["data"]["totals"]["deleted"], 0);
        assert_eq!(event["data"]["applied"], false);
        assert_eq!(event["data"]["dry_run"], false);
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
        // orphan_reap_sweep is ephemeral-class, so retention routing lands the
        // row in the .ephemeral sibling, never in the journal proper.
        let line = std::fs::read_to_string(crate::events::ephemeral_path(&path)).unwrap();
        assert!(line.contains("\"skipped\":true"), "{line}");
        assert!(line.contains("\"candidates\":0"), "{line}");
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
        // Same retention routing as above: the sweep row lives in the sibling.
        let line = std::fs::read_to_string(crate::events::ephemeral_path(&path)).unwrap();
        assert!(line.contains(ORPHAN_SWEEP_EVENT), "{line}");
        assert!(line.contains("\"reaped\":0"), "{line}");
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
            confirm_hold: None,
        }
    }

    /// The planning lane (AC3-HP): a blueprinter named on a node that reached
    /// ready has FINISHED its assignment - the plan was written, the node
    /// moved on, and THIS session's own blueprint row carries `ended_at`.
    /// Quiet past grace retires it without closing the feature or inventing
    /// a node.
    #[test]
    fn ac3_hp_planner_on_ready_node_completes_at_plan_written() {
        let planner = GcRow {
            work: WorkState::Open {
                node: "x-70e1".into(),
                status: "ready".into(),
            },
            planning: Some(vec![("x-70e1".to_string(), "ready".to_string())]),
            planning_closed: vec!["x-70e1".to_string()],
            ..retiring()
        };
        assert_eq!(gc_decide(&planner, GRACE), (GcAction::Retire, None));
        // ...and stays eligible when its node is in flight: the planner is
        // not the implementer.
        let dispatched = GcRow {
            work: WorkState::Open {
                node: "x-70e1".into(),
                status: "in_progress".into(),
            },
            planning: Some(vec![("x-70e1".to_string(), "in_progress".to_string())]),
            planning_closed: vec!["x-70e1".to_string()],
            ..retiring()
        };
        assert_eq!(gc_decide(&dispatched, GRACE), (GcAction::Retire, None));
    }

    /// The planning lane's edge (AC3-EDGE): one named node still at `idea`
    /// holds the planner - the plan it was dispatched to write never landed
    /// there. A planning row with NO planning statuses (graph lost the join)
    /// also keeps: an unjudgeable row is never retired on the planning lane.
    #[test]
    fn ac3_edge_planner_on_idea_node_stays_outstanding() {
        let planner = GcRow {
            work: WorkState::Open {
                node: "x-70e1".into(),
                status: "idea".into(),
            },
            planning: Some(vec![("x-70e1".to_string(), "idea".to_string())]),
            ..retiring()
        };
        assert_eq!(
            gc_decide(&planner, GRACE),
            (
                GcAction::Keep,
                Some(KeepReason::OpenWork {
                    node: "x-70e1".into(),
                    status: "idea".into(),
                })
            )
        );

        let unplannable = GcRow {
            work: WorkState::Open {
                node: "x-70e1".into(),
                status: "ready".into(),
            },
            planning: Some(Vec::new()),
            ..retiring()
        };
        assert_eq!(
            gc_decide(&unplannable, GRACE),
            (
                GcAction::Keep,
                Some(KeepReason::OpenWork {
                    node: "x-70e1".into(),
                    status: "ready".into(),
                })
            )
        );
    }

    /// x-5aef AC4-HP: a `bp-` row whose node reads `ready` but whose OWN
    /// blueprint row carries no `ended_at` keeps its row - the completion
    /// belongs to an earlier assignment, and the reason names the unclosed
    /// node. Fail closed: an absent closed set keeps the row too.
    #[test]
    fn ac4_hp_ready_node_without_a_closed_assignment_holds_the_row() {
        let replanner = GcRow {
            work: WorkState::Open {
                node: "x-5aef".into(),
                status: "ready".into(),
            },
            planning: Some(vec![("x-5aef".to_string(), "ready".to_string())]),
            planning_closed: Vec::new(),
            ..retiring()
        };
        assert_eq!(
            gc_decide(&replanner, GRACE),
            (
                GcAction::Keep,
                Some(KeepReason::PlanningUnclosed {
                    node: "x-5aef".into(),
                })
            )
        );
        // The fail-closed twin: a different node closed, this one not.
        let partial = GcRow {
            planning: Some(vec![
                ("x-5aef".to_string(), "ready".to_string()),
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
                    node: "x-5aef".into(),
                })
            )
        );
    }

    /// x-5aef AC4-EDGE: the same row retires once its own blueprint row
    /// gains `ended_at` - the closed set now names every assigned node.
    #[test]
    fn ac4_edge_a_closed_assignment_releases_the_row() {
        let replanner = GcRow {
            work: WorkState::Open {
                node: "x-5aef".into(),
                status: "ready".into(),
            },
            planning: Some(vec![("x-5aef".to_string(), "ready".to_string())]),
            planning_closed: vec!["x-5aef".to_string()],
            ..retiring()
        };
        assert_eq!(gc_decide(&replanner, GRACE), (GcAction::Retire, None));
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
            name: "target-x-07dc-worker".into(),
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
            index,
            open_do: HashMap::new(),
            phases: HashMap::new(),
            closed_planning: HashMap::new(),
            statuses: HashMap::from([("N1".to_string(), "done".to_string())]),
            pr_state: HashMap::from([("N1".to_string(), (None, 0))]),
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
            &move |_e| {
                flag.store(true, Ordering::SeqCst);
                true
            },
            &|_e| crate::daemon::CascadeOutcome::NotApplicable,
            &no_agents,
            &|_e| (None, None),
            &|_e| None,
        );
        assert!(
            !stopped.load(Ordering::SeqCst),
            "the stop seam fired in dry-run"
        );
        assert_eq!(
            summary.retired.len(),
            1,
            "the row still classifies would-retire"
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
            index,
            open_do: HashMap::new(),
            phases: HashMap::new(),
            closed_planning: HashMap::new(),
            statuses: HashMap::from([("N1".to_string(), "done".to_string())]),
            pr_state: HashMap::from([("N1".to_string(), (None, 0))]),
        }));
        let emitter = crate::events::EventEmitter::new(std::path::PathBuf::new(), "daemon");
        let stopped = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stopped);
        let calls = Cell::new(0u32);
        let calls_ref = &calls;
        let transcript_path = transcript.clone();
        let summary = gc_sweep::run(
            &home,
            &emitter,
            900, // grace
            false,
            7,
            &|_h| graph.borrow_mut().take(),
            // Call 1 (classification): the transcript is 2000s old, past
            // grace. Call 2 (the apply-window re-check): the SAME file reads
            // fresh, as if the session just wrote a turn.
            &move |_e| {
                let n = calls_ref.get();
                calls_ref.set(n + 1);
                if n == 0 {
                    Some(vec![transcript_path.clone()])
                } else {
                    let fresh_file = transcript_path.clone();
                    std::fs::File::options()
                        .write(true)
                        .open(&fresh_file)
                        .unwrap()
                        .set_times(
                            std::fs::FileTimes::new().set_modified(std::time::SystemTime::now()),
                        )
                        .unwrap();
                    Some(vec![fresh_file])
                }
            },
            &move |_e| {
                flag.store(true, Ordering::SeqCst);
                true
            },
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
            &|_| true,
            &|_| crate::daemon::CascadeOutcome::NotApplicable,
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

    #[test]
    fn transcript_age_reads_the_newest_store_match() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old.jsonl");
        let fresh = dir.path().join("fresh.jsonl");
        std::fs::write(&old, "{}").unwrap();
        std::fs::write(&fresh, "{}").unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let hits = vec![old, fresh];
        let age = transcript_age_s(Some(&hits), now).unwrap();
        assert!(
            age < 5,
            "age {age} should be ~0 for a just-written transcript"
        );
        assert_eq!(transcript_age_s(None, now), None);
        assert_eq!(transcript_age_s(Some(&[]), now), None);
    }
}
