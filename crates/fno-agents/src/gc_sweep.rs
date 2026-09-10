//! The retirement sweep (x-c672): one pass, stop then drop.
//!
//! The pure row policy is `gc::gc_decide`; the pure tree policy is
//! `gc::tree_action`. This module owns their I/O: the settle
//! (`settle_stale_do_rows`) writes the graph first, the row pass reads what it
//! wrote; then the graph read that feeds the reverse join, the served transcript mtime, the confirmed
//! stop of a held process, the reap receipt every removal stages before the
//! row drops, the registry write under its `created_at` TOCTOU guard, and the
//! worktree prune for a clean-and-merged tree.
//!
//! The settle (`settle_stale_do_rows`) writes the graph before the row pass:
//! an open do row on a done, merged node with no open additional PR has
//! nothing left to re-open, so the sweep fills `ended_at` and KEEPS the row -
//! the session provenance (phase, harness, session id, started_at) survives,
//! stamped `ended_by: "reap-sweep"` because the sweep infers the end instant
//! rather than observing it. A row the settle cannot fill on a node still in
//! flight keeps under `open do row on done node`.
//!
//! Retirement removes the session from its harness's ACTIVE surface only
//! (the agent list, the session index); the native history is never deleted,
//! and neither is a branch. The node's `sessions[]` row and the transcript
//! survive the retirement, so `fno agents resume` still opens the session
//! afterwards.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{json, Value};

use crate::events::EventEmitter;
use crate::gc::{
    gc_decide, row_handle, transcript_age_s, tree_action, GcAction, GcRow, KeepReason, TreeAction,
};
use crate::graph_store::{self, WorkState};
use crate::node_route;
use crate::paths::AgentsHome;
use crate::receipt::{
    build_reap_receipt, expire_receipt_details, write_reap_receipt, EffectRecord, ReapReceipt,
};
use crate::state;

pub(crate) use crate::gc_inventory::HarnessStoreIndex;

/// Outcome of one retirement pass, for the `fno agents reap` report and
/// tests. Every row the pass judged lands in exactly one bucket, zero
/// counts included - a pass that names nothing is indistinguishable from a
/// pass that never ran.
#[derive(Debug, Default, PartialEq)]
pub struct GcSummary {
    /// `(row id, basis)` for every row retired: "every named node done: N1".
    pub retired: Vec<(String, String)>,
    /// `(row id, worktree path)` for every tree pruned (clean and merged;
    /// the branch survives).
    pub pruned: Vec<(String, String)>,
    /// `(row id, reason)`: the sweep asked `prune_tree` to remove a
    /// clean-and-merged tree and the attempt did not confirm removal - the
    /// gate refused, the probe could not answer, or `git worktree remove`
    /// itself failed. A row here never also appears in `pruned` - the
    /// callback's own answer decides the bucket, not the order that asked.
    pub prune_failed: Vec<(String, String)>,
    /// `(row id, holder)`: a retiring row's cwd is still occupied by
    /// `holder`, a live registry row not retiring this pass - the tree
    /// survives and the prune never runs.
    pub kept_shared_tree: Vec<(String, String)>,
    /// `(row id, descendant)`: a live registry row names this row's session
    /// in its own `spawned_by_session` - the parent is held, unretired,
    /// until that child is gone.
    pub kept_live_descendants: Vec<(String, String)>,
    pub kept_operator: Vec<String>,
    pub kept_crowned: Vec<String>,
    /// `(id, origin)`: origin is not `spawn` (adopted, unknown spelling), so
    /// a sweep never removes it - only a row fno itself spawned retires.
    pub kept_not_spawn: Vec<(String, String)>,
    /// Named in no node's `sessions[]`: no provenance, no work-done verdict.
    pub kept_no_provenance: Vec<String>,
    /// `(id, a, b)` (x-5a62): two provenance sources resolved different
    /// nodes, so the row is held rather than retired on a guess.
    pub kept_node_conflict: Vec<(String, String, String)>,
    /// `(id, node, detail)` (x-5a62): the node reads done but its PR state
    /// contradicts - an open additional PR, or a recorded merge_status that
    /// is not `merged`.
    pub kept_pr_contradicts: Vec<(String, String, String)>,
    /// `(id, node)`: the node reads planning-complete but THIS session's own
    /// blueprint/think row on it carries no `ended_at` - the completion
    /// belongs to an earlier assignment, never to this worker (x-5aef).
    pub kept_planning_unclosed: Vec<(String, String)>,
    /// `(id, node, status)`: a named node is not done; the first open one.
    pub kept_open_work: Vec<(String, String, String)>,
    /// `(id, age_s)`: the transcript was written inside the grace window.
    pub kept_active: Vec<(String, i64)>,
    /// The transcript could not be resolved through the row's own store.
    pub kept_transcript_unresolved: Vec<String>,
    /// The graph could not be read this pass. Never a retirement on a failed
    /// read.
    pub kept_graph_unreadable: Vec<String>,
    /// `(id, node)`: all named nodes done, but one carries an OPEN do row
    /// for this session (Locked Decision 1).
    pub kept_open_do_row: Vec<(String, String)>,
    /// `(node, harness, session_id)` for every stale open do row this pass
    /// FILLED. The row stays; only `ended_at` and `ended_by` are added.
    pub settled_do_rows: Vec<(String, String, String)>,
    /// `(node, reason)`: the settle write refused. Named, never silent.
    pub settle_refused: Vec<(String, String)>,
    /// `(id, worktree path)`: the row retired, its tree is dirty and stays.
    pub kept_dirty: Vec<(String, String)>,
    /// `(id, worktree path)`: the row retired, the branch never merged and
    /// the tree stays for a human.
    pub kept_unmerged: Vec<(String, String)>,
    /// `(id, worktree path)`: the cleanliness probe could not answer.
    pub kept_unprobed: Vec<(String, String)>,
    /// `(id, reason)`: the confirmed stop of the held process refused; the
    /// row stays in the registry and is retried next tick.
    pub stop_refused: Vec<(String, String)>,
    /// `(id, reason)`: DRY RUN only. The row would retire on its policy
    /// verdict, but no positive death evidence (terminal roster state, dead
    /// pid) backs it, so whether a real run can confirm its stop is unknowable
    /// without side effects. A dry run that counted these as retirable would
    /// promise rows a real run then refuses (2026-09-08: dry promised nine,
    /// real retired zero).
    pub needs_live_stop: Vec<(String, String)>,
    /// `(id, reason)`: a retirement held because no resumable receipt could
    /// be staged. Unknown never removes - a removal the operator cannot undo
    /// needs at least the record of how to come back.
    pub kept_no_receipt: Vec<(String, String)>,
    /// Receipt filenames expired by the retention window this sweep.
    pub expired_receipts: Vec<String>,
    /// `(receipt filename, reason)` for every receipt the retention sweep
    /// HELD: a failed read is not evidence of age.
    pub kept_receipts: Vec<(String, String)>,
}

/// One state file selected for deletion by the shared age policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StateReapEntry {
    pub path: String,
    pub bytes: u64,
    pub age_s: u64,
}

/// One state file retained because its safety proof was incomplete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StateReapKept {
    pub path: String,
    pub reason: String,
}

/// Structured outcome for one independently retained state-file family.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct StateReapFamilySummary {
    pub scanned: usize,
    pub deleted: usize,
    pub would_delete: usize,
    pub bytes: u64,
    pub oldest_age_s: Option<u64>,
    pub kept: Vec<StateReapKept>,
    pub deleted_entries: Vec<StateReapEntry>,
    pub would_delete_entries: Vec<StateReapEntry>,
}

/// Totals are derived only from the four named families.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct StateReapTotals {
    pub scanned: usize,
    pub deleted: usize,
    pub would_delete: usize,
    pub bytes: u64,
    pub oldest_age_s: Option<u64>,
    pub kept: usize,
}

/// State-file-only sweep outcome. This type cannot represent row retirement,
/// so operator and scheduled callers share the file policy without gaining a
/// path to mutate the live agent registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StateFilesReapSummary {
    pub expired_claims: StateReapFamilySummary,
    pub plan_locks: StateReapFamilySummary,
    pub agent_locks: StateReapFamilySummary,
    pub pr_status_cache: StateReapFamilySummary,
    pub totals: StateReapTotals,
    pub applied: bool,
    pub dry_run: bool,
    pub skip_reason: Option<String>,
}

impl Default for StateFilesReapSummary {
    fn default() -> Self {
        Self {
            expired_claims: StateReapFamilySummary::default(),
            plan_locks: StateReapFamilySummary::default(),
            agent_locks: StateReapFamilySummary::default(),
            pr_status_cache: StateReapFamilySummary::default(),
            totals: StateReapTotals::default(),
            applied: false,
            dry_run: true,
            skip_reason: None,
        }
    }
}

/// The graph read that feeds a sweep: the entries (working graph plus
/// archive), the reverse-join index over them, and the open-do map (`session
/// -> nodes carrying an OPEN do row for it`).
#[derive(Debug, Default, Clone)]
pub struct GraphRead {
    pub index: HashMap<String, Vec<(String, String)>>,
    pub open_do: HashMap<String, Vec<String>>,
    /// Normalized session id -> the phases its sessions[] rows carry. The
    /// planning lane reads this to recognize a planner row (blueprint/think)
    /// that a node's reverse join alone cannot.
    pub phases: HashMap<String, Vec<String>>,
    /// Node id -> stored `status` (x-5a62). The cascade's confirm reads it;
    /// its key set is the id set the name and transcript routes resolve
    /// against, so no second id read exists.
    pub statuses: HashMap<String, String>,
    /// Node id -> (merge_status, additional_prs length) (x-5a62). The
    /// confirm step reads positive PR-state evidence from it; a missing
    /// merge_status is recorded as unrecorded, never asserted unmerged.
    pub pr_state: HashMap<String, (Option<String>, usize)>,
    /// Lowercased session id -> the node ids where THIS session's own
    /// `blueprint` or `think` sessions[] row carries a non-empty `ended_at`
    /// (x-5aef task 1.2). The positive marker the planner's own close
    /// writes; its absence means this assignment never finished.
    pub closed_planning: HashMap<String, std::collections::HashSet<String>>,
}

/// One row the pass decided to retire, with everything the write tail needs.
pub(crate) struct RetireOrder {
    pub(crate) id: String,
    pub(crate) basis: String,
    pub(crate) created_at: String,
    pub(crate) tree: TreeAction,
    pub(crate) worktree: Option<String>,
}

/// Why a row's session effects refused. The caller names its own bucket: the
/// sweep files them under `stop_refused` / `kept_no_receipt` /
/// `kept_open_do_row`, the merge trigger under its `kept` list.
pub(crate) enum RetireRefusal {
    /// The harness stop did not confirm.
    StopRefused(String),
    /// The native active-surface removal did not confirm.
    NativeRemoval(String),
    /// No resumable receipt could be staged.
    NoReceipt(String),
    /// An open do row names this session on a node: an obligation that
    /// opened between the decision and the effects. Checked BEFORE any
    /// effect fires, because a held session whose process was already
    /// stopped is not held at all - it is dead (the codex P1 on PR 1637).
    GraphObligation(String),
}

/// What one commit actually wrote. `retired_names` is the removal truth: a
/// name absent from it kept its row (a replacement session owns the name, or
/// the write failed).
#[derive(Default)]
pub(crate) struct CommitReport {
    pub(crate) retired: Vec<(String, String)>,
    pub(crate) pruned: Vec<(String, String)>,
    pub(crate) prune_failed: Vec<(String, String)>,
    pub(crate) kept_no_receipt: Vec<(String, String)>,
    /// `(row id, holder)`: a shared-cwd occupant not present in `run`'s
    /// snapshot, but live in the registry under the commit lock. Merged
    /// into `GcSummary::kept_shared_tree`.
    pub(crate) kept_shared_tree: Vec<(String, String)>,
    pub(crate) retired_names: std::collections::BTreeSet<String>,
}

/// The state root's graph file: the one `read_graph_entries` reads (plus the
/// advisory archive) and the one the settle writes under the lock.
pub(crate) fn graph_path(home: &AgentsHome) -> PathBuf {
    let state_root = home.root().parent().unwrap_or(home.root());
    state_root.join("graph.json")
}

/// Read the working graph plus the archive. The archive is advisory (a read
/// failure contributes nothing); the WORKING graph failing to parse is `None`
/// and every consumer keeps its rows. A missing graph file is an empty graph,
/// matching the Python read seam.
pub(crate) fn read_graph_entries_raw(home: &AgentsHome) -> Option<Vec<Value>> {
    let graph_path = graph_path(home);
    let state_root = home.root().parent().unwrap_or(home.root());
    let read = |path: &std::path::Path| -> Result<Vec<Value>, ()> {
        match std::fs::read(path) {
            Ok(raw) => serde_json::from_slice::<Value>(&raw)
                .ok()
                .and_then(|v| v.get("entries").cloned())
                .and_then(|e| e.as_array().cloned())
                .ok_or(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(_) => Err(()),
        }
    };
    let mut entries = read(&graph_path).ok()?;
    // The archive: same shape, advisory. An unparseable archive must not
    // blind the sweep to the working graph.
    let archive = read(&state_root.join("graph-archive.json")).unwrap_or_default();
    entries.extend(archive);
    Some(entries)
}

/// Read the working graph plus the archive and build the reverse-join index
/// and the open-do map.
pub fn read_graph_entries(home: &AgentsHome) -> Option<GraphRead> {
    let entries = read_graph_entries_raw(home)?;
    let index = graph_store::sessions_index(&entries);
    let mut open_do: HashMap<String, Vec<String>> = HashMap::new();
    let mut phases: HashMap<String, Vec<String>> = HashMap::new();
    let mut closed_planning: HashMap<String, std::collections::HashSet<String>> = HashMap::new();
    let mut statuses: HashMap<String, String> = HashMap::new();
    let mut pr_state: HashMap<String, (Option<String>, usize)> = HashMap::new();
    for entry in &entries {
        let Some(node_id) = graph_store::entry_id(entry) else {
            continue;
        };
        statuses.insert(
            node_id.to_string(),
            entry
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        );
        pr_state.insert(
            node_id.to_string(),
            (
                entry
                    .get("merge_status")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                entry
                    .get("additional_prs")
                    .and_then(Value::as_array)
                    .map(Vec::len)
                    .unwrap_or(0),
            ),
        );
        let Some(rows) = entry.get("sessions").and_then(Value::as_array) else {
            continue;
        };
        for row in rows {
            let sid = row.get("session_id").and_then(Value::as_str).map(str::trim);
            let Some(sid) = sid.filter(|s| !s.is_empty()) else {
                continue;
            };
            if graph_store::is_open_do_row(row) {
                open_do
                    .entry(sid.to_ascii_lowercase())
                    .or_default()
                    .push(node_id.to_string());
            }
            let phase = row
                .get("phase")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if !phase.is_empty() {
                phases
                    .entry(sid.to_ascii_lowercase())
                    .or_default()
                    .push(phase.clone());
            }
            // The planner's own close receipt (x-5aef task 1.2): a
            // blueprint/think row carrying a non-empty `ended_at` is a
            // finished assignment. `fno backlog session close` stamps it,
            // and Blueprint's finish gate refuses to complete without
            // reading it back - presence binds the completion to THIS
            // session's own work, never to an earlier assignment on the
            // same node.
            if (phase == "blueprint" || phase == "think")
                && row
                    .get("ended_at")
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty())
            {
                closed_planning
                    .entry(sid.to_ascii_lowercase())
                    .or_default()
                    .insert(node_id.to_string());
            }
        }
    }
    Some(GraphRead {
        index,
        open_do,
        phases,
        closed_planning,
        statuses,
        pr_state,
    })
}

/// One open do row the sweep may settle: its node is done, GitHub-confirmed
/// merged, and carries no additional PR whose outcome nothing recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleDoRow {
    pub node: String,
    pub harness: String,
    pub session_id: String,
}

/// Every open do row sitting on a settled node. Every clause is a positive
/// marker: `status == "done"`; `merge_status == "merged"`, a field written
/// only when a caller resolved MERGED from `gh`, so its absence has two
/// explanations and neither is asserted here; and no `additional_prs` entry
/// at all - the graph records no per-entry merge state for an additional PR,
/// so any additional PR holds the row.
pub(crate) fn stale_open_do_rows(entries: &[Value]) -> Vec<StaleDoRow> {
    let mut stale = Vec::new();
    for entry in entries {
        let Some(node_id) = graph_store::entry_id(entry) else {
            continue;
        };
        if entry.get("status").and_then(Value::as_str) != Some("done") {
            continue;
        }
        if entry.get("merge_status").and_then(Value::as_str) != Some("merged") {
            continue;
        }
        let holds_pr = entry
            .get("additional_prs")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty());
        if holds_pr {
            continue;
        }
        for row in entry
            .get("sessions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if graph_store::is_open_do_row(row) {
                stale.push(StaleDoRow {
                    node: node_id.to_string(),
                    harness: row
                        .get("harness")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    session_id: row
                        .get("session_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                });
            }
        }
    }
    stale
}

/// The dry-run settle plan: every stale open do row a real pass would fill.
/// Reads the graph; writes nothing.
pub(crate) fn plan_stale_do_rows(home: &AgentsHome) -> Vec<StaleDoRow> {
    match graph_store::read_defaulted(&graph_path(home), false) {
        Ok(entries) => stale_open_do_rows(&entries),
        Err(_) => Vec::new(),
    }
}

/// Fill `ended_at` on every stale open do row and KEEP the row: on a done,
/// merged node with no open additional PR there is nothing left to re-open,
/// and the session provenance (phase, harness, session id, started_at)
/// survives the retirement question. Returns `(settled, refusals)`; a settle
/// that cannot write names the refusal, never silent, and the sweep retries
/// on its next pass. The stamp records `ended_by: "reap-sweep"` because the
/// sweep INFERS the end instant rather than observing it.
///
/// The read-apply-publish cycle retries a bounded few times before it
/// refuses: `locked_mutate` refuses over ANY foreign write that landed
/// between this read and this write (the guard that makes the write
/// unclobberable), and on a fleet machine one write burst can eat the first
/// attempt. The fill runs fill-if-absent over a fresh read each attempt, so
/// a retry never overwrites an `ended_at` another writer just added.
pub(crate) fn settle_stale_do_rows(home: &AgentsHome) -> (Vec<StaleDoRow>, Vec<(String, String)>) {
    let path = graph_path(home);
    const SETTLE_ATTEMPTS: usize = 5;
    for attempt in 0..SETTLE_ATTEMPTS {
        match settle_attempt(&path) {
            Ok(settled) => return (settled, Vec::new()),
            Err(SettleRefusal::Retry(err)) if attempt + 1 < SETTLE_ATTEMPTS => {
                let _ = err;
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            Err(SettleRefusal::Retry(err)) => {
                let reason =
                    format!("settle write refused: {err} (after {SETTLE_ATTEMPTS} attempts)");
                return (Vec::new(), vec![(String::new(), reason)]);
            }
            Err(SettleRefusal::Fatal(reason)) => {
                return (Vec::new(), vec![(String::new(), reason)])
            }
        }
    }
    unreachable!("every loop arm returns")
}

/// One read-apply-publish attempt. `Err(Retry(_))` is a lost race a fresh
/// read may win; `Err(Fatal(_))` is not.
fn settle_attempt(path: &std::path::Path) -> Result<Vec<StaleDoRow>, SettleRefusal> {
    let base = graph_store::file_content_version(path);
    let mut entries = graph_store::read_defaulted(path, false)
        .map_err(|err| SettleRefusal::Fatal(format!("graph unreadable: {err}")))?;
    let stale = stale_open_do_rows(&entries);
    if stale.is_empty() {
        return Ok(Vec::new()); // nothing stale: never touch the file
    }
    let now = crate::daemon::now_rfc3339_like();
    for row in &stale {
        let Some(entry) = entries
            .iter_mut()
            .find(|e| graph_store::entry_id(e) == Some(row.node.as_str()))
        else {
            continue;
        };
        let Some(sessions) = entry.get_mut("sessions").and_then(Value::as_array_mut) else {
            continue;
        };
        for session in sessions.iter_mut() {
            let matches = graph_store::is_open_do_row(session)
                && session.get("harness").and_then(Value::as_str) == Some(row.harness.as_str())
                && session.get("session_id").and_then(Value::as_str)
                    == Some(row.session_id.as_str());
            if matches {
                if let Some(obj) = session.as_object_mut() {
                    obj.entry("ended_at".to_string())
                        .or_insert_with(|| Value::String(now.clone()));
                    obj.entry("ended_by".to_string())
                        .or_insert_with(|| Value::String("reap-sweep".into()));
                }
            }
        }
    }
    let outcome = graph_store::locked_mutate(
        path,
        graph_store::MutateInput {
            entries,
            // No node crosses into a terminal rung here: the closure-release
            // and board-render gates have nothing to do.
            canonical_path: None,
            base_version: Some(base),
            plan_rungs: None,
        },
        graph_store::DEFAULT_LOCK_TIMEOUT,
    );
    match outcome {
        Ok(_) => Ok(stale),
        // A lost race (the file moved under the snapshot) or a contended
        // lock: a fresh read may win. Anything else is final.
        Err(
            err @ (graph_store::StoreError::Conflict | graph_store::StoreError::LockTimeout(..)),
        ) => Err(SettleRefusal::Retry(err.to_string())),
        Err(err) => Err(SettleRefusal::Fatal(format!("settle write refused: {err}"))),
    }
}

/// Why one settle attempt did not land. A retry is a lost race; a fatal is
/// a named refusal.
enum SettleRefusal {
    Retry(String),
    Fatal(String),
}

/// Drop each planned settle from the dry-run graph read, so the rehearsal
/// reports the outcome the real pass would produce: a planned row no longer
/// counts open.
pub(crate) fn without_settled(mut graph: GraphRead, planned: &[StaleDoRow]) -> GraphRead {
    for row in planned {
        let key = row.session_id.to_ascii_lowercase();
        if let Some(nodes) = graph.open_do.get_mut(&key) {
            nodes.retain(|n| n != &row.node);
            if nodes.is_empty() {
                graph.open_do.remove(&key);
            }
        }
    }
    graph
}

/// Node id -> `(status, merge_status)` over the same read. The merge reaper's
/// doneness re-read: a node must read done AND merged before its worker's
/// rows or tree go.
pub(crate) fn read_graph_node_states(
    home: &AgentsHome,
) -> Option<HashMap<String, (String, Option<String>)>> {
    let entries = read_graph_entries_raw(home)?;
    let mut states = HashMap::new();
    for entry in entries {
        let Some(id) = graph_store::entry_id(&entry) else {
            continue;
        };
        states.insert(
            id.to_string(),
            (
                entry
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                entry
                    .get("merge_status")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            ),
        );
    }
    Some(states)
}

/// Stop a retiring row's held process from a sync caller. The stop is async,
/// so it runs on a dedicated thread with a one-shot current-thread runtime:
/// `Handle::block_on` on the caller's own thread panics inside an ambient
/// runtime (the CLI verb runs under `main`'s `block_on`, the daemon's tick
/// under `spawn_blocking`), and a fresh thread is legal in both. A runtime
/// that cannot be built fails closed: the row keeps under `stop_refused`.
pub(crate) fn stop_row_process(home: &AgentsHome, e: &state::RegistryEntry) -> bool {
    // A claude row owns no worker socket, so the socket probe below reads
    // "down" instantly and the registry row would drop while the claude
    // daemon still holds the session - the adopt-then-rm recovery the
    // operator ran 50 times. Stop through `claude stop` instead.
    if e.harness_name() == "claude" {
        return stop_claude_confirmed(e);
    }
    let home = home.clone();
    let entry = e.clone();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map(|rt| rt.block_on(crate::daemon::stop_worker_confirmed_for_home(&home, &entry)))
            .unwrap_or(false)
    })
    .join()
    .unwrap_or(false)
}

/// Positive death evidence for a claude row, read off the `claude agents
/// --json --all` snapshot. `Some(reason)` proves the session finished - the
/// same standard rm's live gate accepts. A finished claude agent never
/// leaves the roster; it stays listed with state `done`, so absence can
/// never be the proof here. `blocked` is NOT terminal: the row may be
/// rotated and resumed, so it holds.
pub(crate) fn claude_death_reason(
    e: &state::RegistryEntry,
    agents: &crate::claude_roster::ClaudeAgentsSnapshot,
) -> Option<String> {
    if e.harness_name() != "claude" {
        return None;
    }
    let row_id = crate::daemon::claude_row_id(e)?;
    let row = agents.find(&row_id)?;
    if let Some(state) = row
        .state
        .as_deref()
        .filter(|state| crate::claude_roster::is_terminal_roster_state(state))
    {
        return Some(format!("row {row_id} present, state {state}"));
    }
    if let Some(pid) = row.pid {
        // ESRCH or nothing: a failed lookup is not death, so the verdict
        // needs the existence-specific probe, not start_time's conflated
        // None (two Nones also prove a persistent failure).
        if crate::daemon::pid_is_gone(pid) {
            return Some(format!("row {row_id} pid {pid} is gone"));
        }
    }
    None
}

/// Stop a claude row's session before the row drops. The roster is the exited
/// proof: a session the live roster no longer lists is already gone, and
/// running `claude stop` on it would fail on every future sweep, wedging the
/// row in `stop_refused` forever. A roster read that FAILS holds the row -
/// a torn read is not an exited proof. An unreachable session id (no short
/// id, no session id) holds too: the sweep cannot reach the session, so it
/// must not drop the row and orphan the sideline entry. A successful stop
/// EXIT is a receipt, not a proof: the roster has been seen still listing a
/// session half a minute after a "stopped" return, so absence AFTER the stop
/// is the confirmation and anything else holds the row for the next pass.
fn stop_claude_confirmed(e: &state::RegistryEntry) -> bool {
    let Some(short) = e
        .transport_short()
        .map(str::to_string)
        .or_else(|| roster_short(&e.harness_session_id))
    else {
        return false;
    };
    let sid = e.harness_session_id.as_deref();
    if roster_lists(&short, sid) == Some(false) {
        return true;
    }
    let stopped = {
        let short = short.clone();
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map(|rt| {
                    rt.block_on(async {
                        matches!(
                            crate::daemon::bounded_claude_stop(&short, std::time::Duration::from_secs(15))
                                .await,
                            Ok(Ok(output)) if output.status.success()
                        )
                    })
                })
                .unwrap_or(false)
        })
        .join()
        .unwrap_or(false)
    };
    stopped && roster_lists(&short, sid) == Some(false)
}

/// Whether the live roster still lists the session, by short id or session
/// id. `None` when the roster cannot be read: a torn read is not an exited
/// proof in either direction.
fn roster_lists(short: &str, sid: Option<&str>) -> Option<bool> {
    roster_lists_in(&crate::claude_roster::default_roster_path(), short, sid)
}

fn roster_lists_in(path: &std::path::Path, short: &str, sid: Option<&str>) -> Option<bool> {
    let roster = crate::claude_roster::ClaudeRoster::load(path).ok()?;
    Some(roster.find(short).is_some() || sid.is_some_and(|sid| roster.find(sid).is_some()))
}

/// Resolve a claude short id from the live roster by session id.
fn roster_short(session_id: &Option<String>) -> Option<String> {
    let sid = session_id.as_deref()?.trim();
    if sid.is_empty() {
        return None;
    }
    let roster = crate::claude_roster::ClaudeRoster::load_default().ok()?;
    roster.find(sid).map(|w| w.short_id().to_string())
}

/// The production tree probes for a retiring row: cleanliness first, the
/// merge check only when clean, asked separately because the shared
/// `worktree_gate` door folds the two answers into one verdict.
pub(crate) fn production_tree_probe(e: &state::RegistryEntry) -> (Option<bool>, Option<bool>) {
    if !crate::daemon::is_linked_worktree(&e.cwd) {
        return (None, None);
    }
    let clean = crate::daemon::worktree_clean_probe(&e.cwd);
    if clean != Some(true) {
        return (clean, None);
    }
    (clean, crate::daemon::branch_merged(&e.cwd))
}

/// The one provenance verdict (x-5a62): the reverse join stays first and
/// unchanged (x-c672); only a NoProvenance verdict reaches the cascade,
/// which tries the registry field, the row name, and the transcript,
/// records which source answered, and holds the row when two witnesses
/// disagree. The confirm step reads positive PR-state evidence on every
/// retire-eligible row whichever source answered: an open additional PR
/// holds, a RECORDED merge_status that is not `merged` holds, an ABSENT
/// merge_status does not hold - its absence rides the basis as unrecorded,
/// visible for audit. The registry sweep and the roster-side sweep share
/// this spelling; a second implementation would let the two sweeps
/// disagree about which rows are dead.
pub struct ProvenanceVerdict {
    pub work: WorkState,
    pub route: node_route::NodeRoute,
    pub hold: Option<KeepReason>,
    pub merge_note: Vec<String>,
}

pub fn provenance_verdict(
    e: &state::RegistryEntry,
    sid: &str,
    graph: &GraphRead,
    transcripts: Option<&[std::path::PathBuf]>,
) -> ProvenanceVerdict {
    let mut work = graph_store::work_state(&graph.index, sid);
    // The full cascade runs EVEN WHEN the reverse join answers: the later
    // sources are witnesses, not substitutes, so a source naming a
    // DIFFERENT node holds the row instead of the answer riding on the
    // first witness alone. When the join answers, the work verdict stays
    // the join's own multi-row read (every node the session names must be
    // done); only a NoProvenance route re-derives work from the resolved
    // node's stored status.
    let mut route = node_route::resolve(e, sid, graph, transcripts);
    if route.conflict.is_some() {
        work = WorkState::NoProvenance;
    } else if !matches!(route.source, Some(node_route::NodeSource::Sessions)) {
        work = route.work_state(&graph.statuses);
    }
    let mut hold = route
        .conflict
        .clone()
        .map(|(src, node)| KeepReason::NodeConflict {
            a: src.as_str().to_string(),
            b: node,
        });
    let mut merge_note: Vec<String> = Vec::new();
    if let WorkState::AllDone { nodes } = &work {
        for node in nodes {
            let (merge_status, extra) = graph.pr_state.get(node).cloned().unwrap_or((None, 0));
            if extra > 0 {
                hold = Some(KeepReason::PrStateContradicts {
                    node: node.clone(),
                    detail: format!("additional_prs: {extra}"),
                });
                break;
            }
            match &merge_status {
                Some(m) if m != "merged" => {
                    hold = Some(KeepReason::PrStateContradicts {
                        node: node.clone(),
                        detail: format!("merge_status: {m}"),
                    });
                    break;
                }
                Some(m) => merge_note.push(format!("{node}:{m}")),
                None => merge_note.push(format!("{node}:unrecorded")),
            }
        }
    }
    ProvenanceVerdict {
        work,
        route,
        hold,
        merge_note,
    }
}

/// The one retirement pass. Every I/O seam (`read_graph`, `store_matches`,
/// `stop_confirmed`, `tree_probe`, `prune_tree`) is injected so a test
/// stages the world; production wiring is [`crate::gc::gc_sweep`] /
/// [`crate::gc::gc_sweep_dry_run`]. `agents_read` is the same kind of seam
/// for the `claude agents --json --all` snapshot: read at most once per
/// sweep, lazily, only when a row actually reaches the stop gate - steady
/// state keeps zero subprocesses on the hot path.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run(
    home: &AgentsHome,
    emitter: &EventEmitter,
    grace_secs: i64,
    dry_run: bool,
    retain_days: u64,
    read_graph: &dyn Fn(&AgentsHome) -> Option<GraphRead>,
    store_matches: &dyn Fn(&state::RegistryEntry) -> Option<Vec<PathBuf>>,
    stop_confirmed: &dyn Fn(&state::RegistryEntry) -> bool,
    surface_removal: &dyn Fn(&state::RegistryEntry) -> crate::daemon::CascadeOutcome,
    agents_read: &dyn Fn() -> crate::claude_roster::ClaudeAgentsSnapshot,
    tree_probe: &dyn Fn(&state::RegistryEntry) -> (Option<bool>, Option<bool>),
    prune_tree: &dyn Fn(&state::RegistryEntry) -> Option<crate::daemon::PruneOutcome>,
) -> GcSummary {
    let mut summary = GcSummary::default();
    // The retention pass runs on EVERY sweep, before the empty-registry early
    // return: receipts age out on their own clock. Any receipt this pass goes
    // on to write carries `reaped_at` of now, so it can never be this
    // expiry's victim.
    if !dry_run {
        expire_reap_receipts(home, retain_days, &mut summary);
    }
    let registry = state::load_registry(&home.registry_json()).unwrap_or_default();
    if registry.entries.is_empty() {
        return summary; // empty registry -> nothing to sweep
    }
    let graph = read_graph(home);
    let now = crate::daemon::now_epoch_secs();
    // One ledger parse per sweep: every receipt's enrichment reads these rows.
    let ledger = ledger_rows(&default_ledger_path());
    let mut receipts: std::collections::BTreeMap<String, ReapReceipt> =
        std::collections::BTreeMap::new();
    // Keyed by row name -> the `created_at` we evaluated. Applied under the
    // lock ONLY when the row's current `created_at` still matches, so a
    // same-name session recreated between this snapshot and the write is
    // never clobbered by a stale name-only decision (TOCTOU).
    let mut to_retire: std::collections::BTreeMap<String, RetireOrder> =
        std::collections::BTreeMap::new();
    // The agents snapshot is read at most once per sweep, on the first row
    // that reaches the stop gate - never on the empty/kept hot path.
    let agents_memo: std::cell::RefCell<Option<crate::claude_roster::ClaudeAgentsSnapshot>> =
        std::cell::RefCell::new(None);

    for e in &registry.entries {
        let id = row_handle(e);
        if e.origin.as_deref() == Some("operator") {
            summary.kept_operator.push(id);
            continue;
        }
        if e.crown_level.is_some() {
            summary.kept_crowned.push(id);
            continue;
        }
        // The origin gate runs BEFORE the graph read so a row fno never
        // spawned is named by its own gate whatever the graph's state - the
        // policy's own order (gc_decide checks origin first), not shadowed by
        // kept_graph_unreadable.
        if e.origin.as_deref() != Some("spawn") {
            summary
                .kept_not_spawn
                .push((id, e.origin.clone().unwrap_or_default()));
            continue;
        }
        let Some(graph) = &graph else {
            summary.kept_graph_unreadable.push(id);
            continue;
        };
        let sid = e.harness_session_id.as_deref().unwrap_or("").trim();
        let verdict = provenance_verdict(e, sid, graph, store_matches(e).as_deref());
        let work = verdict.work;
        // Locked Decision 1: every named node done but one still carries an
        // OPEN do row for this session -> the row stays and the node is
        // named. The retirement never settles graph rows itself.
        if matches!(work, WorkState::AllDone { .. }) {
            if let Some(nodes) = graph.open_do.get(&sid.to_ascii_lowercase()) {
                let node = nodes.first().cloned().unwrap_or_default();
                summary.kept_open_do_row.push((id, node));
                continue;
            }
        }
        let confirm_hold = verdict.hold;
        let merge_note = verdict.merge_note;
        let age = transcript_age_s(store_matches(e).as_deref(), now);
        let owns_worktree = !e.is_one_shot_ask() && crate::daemon::is_linked_worktree(&e.cwd);
        // The planning lane (x-70e1 task 2): a blueprint/think row's OWN job
        // ends at plan-written-and-node-ready. A row whose sessions[] phases
        // name it a planner (or whose dispatch label is the bp- shape) gets
        // its every named node's status checked as a set; any node still at
        // `idea` (the plan never landed) or similar holds the row.
        // x-5aef task 1.2 binds the verdict to the CURRENT assignment: the
        // statuses come paired with their node ids, and the set of nodes
        // THIS session closed (its own blueprint/think row carrying a
        // non-empty ended_at) rides beside them. A quiet replanning worker
        // dispatched onto a node a previous blueprint moved to `ready`
        // inherits no completion it did not write.
        let is_planning = graph
            .phases
            .get(&sid.to_ascii_lowercase())
            .is_some_and(|phases| phases.iter().any(|p| p == "blueprint" || p == "think"))
            || e.name.starts_with("bp-");
        let planning = if is_planning {
            Some(
                graph
                    .index
                    .get(&sid.to_ascii_lowercase())
                    .cloned()
                    .unwrap_or_default(),
            )
        } else {
            None
        };
        let planning_closed = if is_planning {
            graph
                .closed_planning
                .get(&sid.to_ascii_lowercase())
                .map(|set| set.iter().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let row = GcRow {
            origin: e.origin.clone(),
            crowned: e.crown_level.is_some(),
            work,
            transcript_age_s: age,
            owns_worktree,
            worktree_clean: None,
            branch_merged: None,
            planning,
            planning_closed,
            confirm_hold,
        };
        let (action, reason) = gc_decide(&row, grace_secs);
        if action == GcAction::Keep {
            match reason {
                Some(KeepReason::Operator) => summary.kept_operator.push(id),
                Some(KeepReason::Crowned) => summary.kept_crowned.push(id),
                Some(KeepReason::NotSpawn { origin }) => summary.kept_not_spawn.push((id, origin)),
                Some(KeepReason::NoProvenance) => summary.kept_no_provenance.push(id),
                Some(KeepReason::OpenWork { node, status }) => {
                    summary.kept_open_work.push((id, node, status))
                }
                Some(KeepReason::Active { age_s }) => summary.kept_active.push((id, age_s)),
                Some(KeepReason::TranscriptUnresolved) => {
                    summary.kept_transcript_unresolved.push(id)
                }
                Some(KeepReason::NodeConflict { a, b }) => {
                    summary.kept_node_conflict.push((id, a, b))
                }
                Some(KeepReason::PrStateContradicts { node, detail }) => {
                    summary.kept_pr_contradicts.push((id, node, detail))
                }
                Some(KeepReason::PlanningUnclosed { node }) => {
                    summary.kept_planning_unclosed.push((id, node))
                }
                // GraphUnreadable / OpenDoRow are decided above, before the
                // policy ran; they cannot arrive here.
                _ => {}
            }
            continue;
        }
        // A parent whose descendant is still live is never retired - the
        // lineage field says who spawned whom, and this is the only site
        // that consults it. Runs before staging so no active-surface
        // removal ever touches a row a live child names.
        if !sid.is_empty() {
            let sid_lower = sid.to_ascii_lowercase();
            if let Some(child) = registry.entries.iter().find(|other| {
                other.name != e.name
                    && other
                        .spawned_by_session
                        .as_deref()
                        .is_some_and(|s| s.trim().to_ascii_lowercase() == sid_lower)
            }) {
                summary.kept_live_descendants.push((id, row_handle(child)));
                continue;
            }
        }
        // A retiring row first confirms its process is stopped: a refusal
        // keeps the row this tick and names the refusal. DRY-RUN never stops
        // anything - a rehearsal that killed the worker it rehearsed
        // retiring would be the destructive run wearing a dry flag.
        // Per-candidate freshness re-check (the fold of the reap-guard
        // finding): between classifying and applying, re-read THIS row's
        // transcript activity. Activity arrived in the window -> keep, and a
        // re-read that cannot resolve (a moved or deleted transcript) keeps
        // too: absence on re-read is not quiet. One stat, on rows already
        // classified would-retire, so the hot path pays nothing.
        if !dry_run {
            let fresh_age = transcript_age_s(store_matches(e).as_deref(), now);
            let still_quiet = matches!(fresh_age, Some(a) if a > grace_secs);
            if !still_quiet {
                let age_now = fresh_age.unwrap_or(0);
                summary.kept_active.push((id, age_now));
                continue;
            }
        }
        //
        // Positive death evidence decides BEFORE any stop is attempted, off
        // one lazy snapshot read per sweep (claude rows only: the evidence
        // instrument is claude's roster). A finished claude agent never
        // leaves the roster, so for it the stop's own absence-confirmation
        // can never arrive; the evidence is the proof instead.
        let death = if e.harness_name() == "claude" {
            let mut memo = agents_memo.borrow_mut();
            let snapshot = memo.get_or_insert_with(&agents_read);
            claude_death_reason(e, snapshot)
        } else {
            None
        };
        // The 2026-09-08 shape (dry promised nine, real retired zero): a dry
        // run never promises a claude stop it holds no evidence for.
        if dry_run && death.is_none() && e.harness_name() == "claude" {
            summary.needs_live_stop.push((
                id,
                "no terminal roster state and no dead pid; a dry run does not \
                 promise a stop it cannot prove"
                    .into(),
            ));
            continue;
        }
        // Death evidence satisfies the stop. It wraps the callee's seam here,
        // in the caller that owns the snapshot, so the shared signature is
        // untouched.
        let stop_on_death = |entry: &state::RegistryEntry| death.is_some() || stop_confirmed(entry);
        if let Err(refusal) = stage_session_retirement(
            home,
            e,
            ledger.as_deref(),
            dry_run,
            &stop_on_death,
            surface_removal,
            &mut receipts,
        ) {
            match refusal {
                RetireRefusal::StopRefused(reason) => {
                    // A claude row with no death evidence names the missing
                    // evidence, not just the unconfirmed stop: the refusal
                    // says what would have satisfied it.
                    let reason = if death.is_none() && e.harness_name() == "claude" {
                        "no death evidence (no terminal roster state, no dead pid) and the \
                         stop did not confirm; row kept for retry"
                            .into()
                    } else {
                        reason
                    };
                    summary.stop_refused.push((id, reason));
                }
                RetireRefusal::NativeRemoval(reason) => summary.stop_refused.push((id, reason)),
                RetireRefusal::NoReceipt(reason) => summary.kept_no_receipt.push((id, reason)),
                RetireRefusal::GraphObligation(node) => summary.kept_open_do_row.push((id, node)),
            }
            continue;
        }
        // The tree probes run only now, on a row already retiring: steady
        // state has no such rows, so no subprocess runs on the hot path.
        let mut probed = row;
        if owns_worktree {
            let (clean, merged) = tree_probe(e);
            probed.worktree_clean = clean;
            probed.branch_merged = merged;
        }
        let tree = tree_action(&probed);
        // The retire basis names the route (x-5a62): a retirement nobody can
        // audit is the failure this string prevents. Every AllDone row gets
        // the audit line - the sessions route included - so one policy has
        // one spelling.
        let via = verdict
            .route
            .source
            .unwrap_or(node_route::NodeSource::Sessions);
        let basis = match &probed.work {
            WorkState::AllDone { nodes } => {
                let named = format!("every named node done: {}", nodes.join(", "));
                let mut note = format!("via {}", via.as_str());
                if !verdict.route.agreeing.is_empty() {
                    let names: Vec<&str> = verdict
                        .route
                        .agreeing
                        .iter()
                        .map(node_route::NodeSource::as_str)
                        .collect();
                    note.push_str(&format!(", agreeing: {}", names.join(", ")));
                }
                note.push_str(&format!("; merge_status: {}", merge_note.join(", ")));
                format!("{named} ({note})")
            }
            _ => "done".to_string(), // unreachable: only AllDone retires
        };
        let worktree = if probed.owns_worktree {
            Some(e.cwd.clone())
        } else {
            None
        };
        match tree {
            TreeAction::KeepDirty => summary.kept_dirty.push((id.clone(), e.cwd.clone())),
            TreeAction::KeepUnmerged => summary.kept_unmerged.push((id.clone(), e.cwd.clone())),
            TreeAction::KeepUnprobed => summary.kept_unprobed.push((id.clone(), e.cwd.clone())),
            _ => {}
        }
        to_retire.insert(
            e.name.clone(),
            RetireOrder {
                id,
                basis,
                created_at: e.created_at.clone(),
                tree,
                worktree,
            },
        );
    }

    // A cwd another live row still occupies is never pruned out from under
    // it, and two rows retiring on the SAME cwd this pass still prune it
    // exactly once - `owns_worktree` above answers only "does THIS row's
    // own cwd look like a linked worktree", nothing about who else sits
    // there.
    let mut prune_by_cwd: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for (name, order) in to_retire.iter() {
        if order.tree == TreeAction::Prune {
            if let Some(cwd) = &order.worktree {
                prune_by_cwd
                    .entry(cwd.clone())
                    .or_default()
                    .push(name.clone());
            }
        }
    }
    for (cwd, names) in prune_by_cwd {
        // `min_by_key` over `registry.entries` (unordered) rather than
        // `find`: with two or more live occupants, the reported holder name
        // must be deterministic across runs, not whichever the vec order
        // happens to surface first.
        let occupant = registry
            .entries
            .iter()
            .filter(|e| e.cwd == cwd && !to_retire.contains_key(&e.name))
            .min_by_key(|e| &e.name);
        if let Some(occupant) = occupant {
            let holder = row_handle(occupant);
            for name in names {
                if let Some(order) = to_retire.get_mut(&name) {
                    order.tree = TreeAction::None;
                    summary
                        .kept_shared_tree
                        .push((order.id.clone(), holder.clone()));
                }
            }
        } else if names.len() > 1 {
            // Both rows retire together: the tree goes with the first, the
            // rest own nothing left to prune.
            for name in names.iter().skip(1) {
                if let Some(order) = to_retire.get_mut(name) {
                    order.tree = TreeAction::None;
                }
            }
        }
    }

    if to_retire.is_empty() {
        return summary;
    }
    if dry_run {
        for order in to_retire.values() {
            summary
                .retired
                .push((order.id.clone(), order.basis.clone()));
            if order.tree == TreeAction::Prune {
                if let Some(path) = &order.worktree {
                    summary.pruned.push((order.id.clone(), path.clone()));
                }
            }
        }
        return summary;
    }

    let report = commit_retirements(
        home,
        emitter,
        "gc_sweep",
        &registry.entries,
        &mut to_retire,
        &receipts,
        prune_tree,
    );
    summary.retired = report.retired;
    summary.pruned = report.pruned;
    summary.prune_failed = report.prune_failed;
    summary.kept_no_receipt.extend(report.kept_no_receipt);
    summary.kept_shared_tree.extend(report.kept_shared_tree);
    summary
}

/// The SESSION half of one retirement, shared by the scheduled sweep and the
/// merge trigger so exactly one sequence exists: build and PERSIST the
/// resumable receipt first, then confirm the stop, then apply the native
/// ACTIVE-SURFACE removal, appending each typed effect as it lands
/// (x-5aef task 1.1). Preserve-before-effects: a crash after an effect has
/// run leaves a receipt on disk naming it, instead of a removal nothing
/// recorded.
///
/// The stop refusal keeps the row for retry, and so does a `failed` or `kept`
/// (unverified) native outcome: a retirement applies only when every
/// applicable effect positively confirmed (or measured not-applicable). A
/// refusal path still rewrites the receipt it staged, so the on-disk record
/// carries the effect that refused.
/// DRY-RUN stops nothing and applies nothing - a rehearsal that killed the
/// worker it rehearsed retiring would be the destructive run wearing a dry
/// flag - but it still stages the receipt, so the rehearsal reports the same
/// holds the real run would.
pub(crate) fn stage_session_retirement(
    home: &AgentsHome,
    e: &state::RegistryEntry,
    ledger_rows: Option<&[Value]>,
    dry_run: bool,
    stop_confirmed: &dyn Fn(&state::RegistryEntry) -> bool,
    surface_removal: &dyn Fn(&state::RegistryEntry) -> crate::daemon::CascadeOutcome,
    receipts: &mut std::collections::BTreeMap<String, ReapReceipt>,
) -> Result<(), RetireRefusal> {
    let ledger = ledger_rows
        .and_then(|rows| ledger_entry_in(rows, e.harness_session_id.as_deref().unwrap_or("")));
    // The obligation re-check runs HERE, before any effect: an open do row
    // naming this session that opened since the decision means fresh work
    // was assigned, and stopping the session would kill it while the commit
    // gate "holds" a corpse. One graph read per staged row; staging only
    // happens on rows already classified would-retire, so steady state pays
    // nothing. The commit-level re-check remains as the second belt for the
    // effects-to-registry-drop span, where holding is harmless.
    if !dry_run {
        if let Some(graph) = read_graph_entries(home) {
            let sid = e
                .harness_session_id
                .as_deref()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            if let Some(nodes) = graph.open_do.get(&sid) {
                if let Some(node) = nodes.first().cloned() {
                    return Err(RetireRefusal::GraphObligation(node));
                }
            }
        }
    }
    // The record precedes the effects: a receipt that cannot be built or
    // persisted refuses BEFORE the harness is touched, so no effect ever
    // fires without its recovery record already on disk (AC3-EDGE).
    let mut receipt = match build_reap_receipt(e, ledger) {
        Ok(receipt) => receipt,
        Err(reason) => return Err(RetireRefusal::NoReceipt(reason)),
    };
    if dry_run {
        receipts.insert(e.name.clone(), receipt);
        return Ok(());
    }
    if let Err(err) = write_reap_receipt(home, &receipt) {
        return Err(RetireRefusal::NoReceipt(format!(
            "receipt did not persist: {err}"
        )));
    }
    // Effect 1: the confirmed stop of the held process.
    let stopped = stop_confirmed(e);
    receipt
        .effects
        .push(crate::gc_native::stop_outcome_effect(stopped));
    if !stopped {
        let _ = write_reap_receipt(home, &receipt);
        return Err(RetireRefusal::StopRefused(
            "the stop did not confirm; row kept for retry".into(),
        ));
    }
    // Effect 2: the ACTIVE-SURFACE removal (x-70e1 task 3): claude's agent
    // list, codex's session index, cursor-agent's worker servers - through
    // the same cascade `rm` walks, typed outcome recorded.
    let outcome = surface_removal(e);
    let applied = outcome.satisfies_applied();
    receipt
        .effects
        .push(outcome.effect_record("active-surface"));
    if !applied {
        let _ = write_reap_receipt(home, &receipt);
        return Err(RetireRefusal::NativeRemoval(
            "the native active-surface removal did not confirm".into(),
        ));
    }
    // Effect 3: the resumability evidence, measured off the receipt itself.
    receipt.effects.push(resume_evidence_effect(&receipt));
    let _ = write_reap_receipt(home, &receipt);
    receipts.insert(e.name.clone(), receipt);
    Ok(())
}

/// The resume-evidence op, measured off the staged receipt: the resume
/// tokens are present AND at least one located transcript exists on disk.
/// A `failed` outcome does not hold the row (the session is already
/// stopped); it marks the receipt unverifiable so the gate refuses it
/// rather than certifying a retirement nothing can recover (AC6-EDGE).
pub(crate) fn resume_evidence_effect(receipt: &ReapReceipt) -> EffectRecord {
    let transcript_exists = receipt.native_locator.as_ref().is_some_and(|loc| {
        loc.get("transcripts")
            .and_then(Value::as_array)
            .is_some_and(|paths| {
                paths
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|p| std::path::Path::new(p).exists())
            })
    });
    let confirmed = !receipt.resume_argv.is_empty() && transcript_exists;
    EffectRecord {
        op: "resume-evidence".into(),
        outcome: if confirmed {
            "confirmed-removed".into()
        } else {
            "failed".into()
        },
        detail: None,
        at: crate::daemon::now_rfc3339_like(),
    }
}

/// The WRITE half of a retirement set, shared by the scheduled sweep and the
/// merge trigger: re-check the graph obligation under the commit, persist
/// every receipt, drop the rows under one registry write guarded by
/// `created_at`, then account and emit only for the names the write really
/// removed.
///
/// `caller` names the emitter's error op so a failed write says which door it
/// came through. `to_retire` is drained of every order whose receipt refused
/// to persist: the receipt is the recovery path, so no receipt means no
/// removal.
pub(crate) fn commit_retirements(
    home: &AgentsHome,
    emitter: &EventEmitter,
    caller: &str,
    entries: &[state::RegistryEntry],
    to_retire: &mut std::collections::BTreeMap<String, RetireOrder>,
    receipts: &std::collections::BTreeMap<String, ReapReceipt>,
    prune_tree: &dyn Fn(&state::RegistryEntry) -> Option<crate::daemon::PruneOutcome>,
) -> CommitReport {
    let mut report = CommitReport::default();
    // The obligation re-check (x-5aef task 1.3): between the decision and
    // this write, a node can gain an OPEN do row naming one of these
    // sessions - the decision's evidence is stale by exactly the age of the
    // graph read. One extra read per commit, and a commit only happens when
    // rows are actually retiring, so steady state pays nothing. A failed
    // re-read keeps every row: a read that cannot answer is not evidence
    // the obligation is gone.
    match read_graph_entries(home) {
        None => {
            for order in to_retire.values() {
                report.kept_no_receipt.push((
                    order.id.clone(),
                    "graph unreadable at commit; every row kept".to_string(),
                ));
            }
            to_retire.clear();
            return report;
        }
        Some(graph) => {
            let held: Vec<(String, String)> = to_retire
                .iter()
                .filter_map(|(name, _)| {
                    let entry = entries.iter().find(|e| &e.name == name)?;
                    let sid = entry
                        .harness_session_id
                        .as_deref()?
                        .trim()
                        .to_ascii_lowercase();
                    graph
                        .open_do
                        .get(&sid)
                        .and_then(|nodes| nodes.first().cloned())
                        .map(|node| (name.clone(), node))
                })
                .collect();
            for (name, node) in held {
                if let Some(order) = to_retire.remove(&name) {
                    report.kept_no_receipt.push((
                        order.id,
                        format!("graph obligation opened after the decision: {node}"),
                    ));
                }
            }
        }
    }
    // Persist every receipt BEFORE the write drops its row: the ordering IS
    // the losslessness. A receipt that will not write holds its row for the
    // next sweep instead.
    to_retire.retain(|name, _| {
        let Some(receipt) = receipts.get(name) else {
            report
                .kept_no_receipt
                .push((name.clone(), "no staged receipt".to_string()));
            return false;
        };
        match write_reap_receipt(home, receipt) {
            Ok(()) => true,
            Err(err) => {
                let id = if receipt.short_id.is_empty() {
                    receipt.row_name.clone()
                } else {
                    receipt.short_id.clone()
                };
                report
                    .kept_no_receipt
                    .push((id, format!("receipt did not persist: {err}")));
                false
            }
        }
    });
    if to_retire.is_empty() {
        return report;
    }
    // Names actually removed under the lock (identity still matched), so the
    // emit + summary report only what really happened.
    let retiring: std::collections::BTreeSet<String> = to_retire.keys().cloned().collect();
    let write = state::update_registry(&home.registry_json(), |r| {
        // Revalidate shared-cwd occupancy against the registry as it
        // stands right now, under the lock: `run`'s snapshot is
        // stop-confirmation, receipt-write, and probe seconds old by the
        // time a prune is about to fire, and a newly registered agent on
        // that cwd is invisible to a check run against the old snapshot.
        // The same-cwd tie among rows retiring THIS pass was already
        // settled once in `run`; only a row NOT in `to_retire` counts as
        // a fresh occupant here.
        for order in to_retire.values_mut() {
            if order.tree != TreeAction::Prune {
                continue;
            }
            let Some(cwd) = &order.worktree else {
                continue;
            };
            let occupant = r
                .entries
                .iter()
                .filter(|other| &other.cwd == cwd && !retiring.contains(&other.name))
                .min_by_key(|other| &other.name);
            if let Some(occupant) = occupant {
                order.tree = TreeAction::None;
                report
                    .kept_shared_tree
                    .push((order.id.clone(), row_handle(occupant)));
            }
        }
        r.entries.retain(|e| {
            let Some(order) = to_retire.get(&e.name) else {
                return true;
            };
            if order.created_at != e.created_at {
                return true; // a replacement session owns this name now
            }
            report.retired_names.insert(e.name.clone());
            false
        });
    });
    match write {
        Ok(()) => {
            for e in entries {
                let Some(order) = to_retire.get(&e.name) else {
                    continue;
                };
                if !report.retired_names.contains(&e.name) {
                    continue;
                }
                // Dispatch accounting, unchanged from the exit-stamp era: a
                // removed row that drove a dispatch loop without a recorded
                // termination emits `node_failed` so the failure-streak
                // ledger stays honest; a failed write restores the row.
                let node_id = crate::daemon::dispatch_node_id(&e.name);
                let mut target_session_id = None;
                let mut termination_event = false;
                let mut accounted = true;
                if let Some(node_id) = node_id.as_deref() {
                    match crate::daemon::dispatch_termination(home, e, node_id) {
                        crate::daemon::DispatchTermination::Found(session_id) => {
                            target_session_id = Some(session_id);
                            termination_event = true;
                        }
                        crate::daemon::DispatchTermination::Absent(session_id) => {
                            target_session_id = session_id;
                            if let Err(err) = crate::daemon::record_dead_dispatch(
                                home,
                                e,
                                node_id,
                                target_session_id.as_deref(),
                            ) {
                                accounted = false;
                                let _ = emitter.emit(
                                    "daemon_recovery_error",
                                    &json!({
                                        "op": "record_dead_dispatch",
                                        "short_id": e.short_id,
                                        "error": err,
                                        "restore_error":
                                            crate::daemon::restore_unaccounted_row(home, e).err(),
                                    }),
                                );
                            }
                        }
                        crate::daemon::DispatchTermination::Unknown(err) => {
                            accounted = false;
                            let _ = emitter.emit(
                                "daemon_recovery_error",
                                &json!({
                                    "op": "observe_dead_dispatch_termination",
                                    "short_id": e.short_id,
                                    "error": err,
                                    "restore_error":
                                        crate::daemon::restore_unaccounted_row(home, e).err(),
                                }),
                            );
                        }
                    }
                }
                if !accounted {
                    report.retired_names.remove(&e.name);
                    continue;
                }
                let _ = emitter.emit(
                    "agent_row_reaped",
                    &json!({
                        "short_id": e.short_id,
                        "name": e.name,
                        "node_id": node_id,
                        "session_id": target_session_id,
                        "termination_event": termination_event,
                        "harness": e.harness_name(),
                        "harness_session_id": e.harness_session_id,
                        "basis": order.basis,
                        // Every retirement is a finished-turn shape now: the
                        // receipt and the node's sessions[] row keep the
                        // resumable handle.
                        "resumable": true,
                    }),
                );
                report.retired.push((order.id.clone(), order.basis.clone()));
                if order.tree == TreeAction::Prune {
                    // The same door a human removal walks (production: gate +
                    // merge check + `git worktree remove`; the branch
                    // survives). The callback's own answer decides the
                    // bucket - the order that asked for a prune is not proof
                    // one happened.
                    match prune_tree(e) {
                        Some(crate::daemon::PruneOutcome::Removed(path)) => {
                            report.pruned.push((order.id.clone(), path))
                        }
                        Some(crate::daemon::PruneOutcome::Kept(reason)) => {
                            report.prune_failed.push((order.id.clone(), reason))
                        }
                        None => report
                            .prune_failed
                            .push((order.id.clone(), "the row owns no linked worktree".into())),
                    }
                }
            }
        }
        Err(err) => {
            let _ = emitter.emit(
                "daemon_recovery_error",
                &json!({"op": caller, "error": err.to_string()}),
            );
            // Nothing was removed; report no retirements (no event/disk
            // divergence).
            report.retired.clear();
            report.pruned.clear();
            report.prune_failed.clear();
            report.retired_names.clear();
        }
    }
    report
}

/// Expire receipts older than `retain_days` in the sweep that also writes
/// them: one pass both records and prunes. A receipt whose `reaped_at` is
/// missing or unparseable is KEPT and named: a failed read is not evidence of
/// age, and deleting on one destroys the handle this store exists to
/// preserve.
fn expire_reap_receipts(home: &AgentsHome, retain_days: u64, summary: &mut GcSummary) {
    let dir = home.root().join("reap-receipts");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return; // nothing was ever written: no store, no expiry
    };
    let now = row_timestamp(Some(&Value::String(crate::daemon::now_rfc3339_like())));
    let Some(now) = now else {
        return; // the clock itself unreadable: prune nothing
    };
    let window_secs = retain_days.saturating_mul(86_400);
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                keep_state_file(
                    summary,
                    state_path(shared_root, dir),
                    format!("read entry failed: {error}"),
                );
                continue;
            }
        };
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let read = std::fs::read(&path)
            .map_err(|e| format!("receipt unreadable: {e}"))
            .and_then(|raw| {
                serde_json::from_slice::<ReapReceipt>(&raw)
                    .map_err(|e| format!("receipt malformed: {e}"))
            });
        let mut receipt = match read {
            Ok(receipt) => receipt,
            Err(reason) => {
                summary.kept_receipts.push((name, reason));
                continue;
            }
        };
        let reaped = match row_timestamp(Some(&Value::String(receipt.reaped_at.clone()))) {
            Some(ts) => ts,
            None => {
                summary.kept_receipts.push((
                    name,
                    "reaped_at missing or unparseable; a failed read is not evidence of age"
                        .to_string(),
                ));
                continue;
            }
        };
        let age_secs = (now - reaped).num_seconds().max(0) as u64;
        if age_secs > window_secs {
            // Past the window: strip the expendable detail, keep the identity
            // core. The mapping this store exists to preserve outlives the
            // operation log (AC2-HP); deleting the file would destroy the
            // only recovery record for a session that may still be resumable.
            if expire_receipt_details(&mut receipt) {
                // Rewrite IN PLACE: the expiry's job is to age THIS file's
                // expendable detail, not to mint a second receipt at the
                // canonical key while the original keeps its stale copy.
                let body = match serde_json::to_vec_pretty(&receipt) {
                    Ok(body) => body,
                    Err(err) => {
                        summary
                            .kept_receipts
                            .push((name, format!("expiry rewrite failed: {err}")));
                        continue;
                    }
                };
                match std::fs::write(&path, body) {
                    Ok(()) => summary.expired_receipts.push(name),
                    Err(err) => summary
                        .kept_receipts
                        .push((name, format!("expiry rewrite failed: {err}"))),
                }
            }
            // Nothing expendable left: the receipt is already the identity
            // core only; leave it byte-identical and name it as kept.
            else {
                summary
                    .kept_receipts
                    .push((name, "past retention window; identity core kept".into()));
            }
        }
    }
}

fn lock_name(shared_root: &std::path::Path, path: &std::path::Path) -> String {
    path.strip_prefix(shared_root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

#[cfg(unix)]
fn opened_path_matches(file: &std::fs::File, path: &std::path::Path) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt;

    let opened = file.metadata()?;
    let current = std::fs::symlink_metadata(path)?;
    Ok((opened.dev(), opened.ino()) == (current.dev(), current.ino()))
}

#[cfg(not(unix))]
fn opened_path_matches(_file: &std::fs::File, _path: &std::path::Path) -> std::io::Result<bool> {
    Ok(true)
}

fn state_path(shared_root: &std::path::Path, path: &std::path::Path) -> String {
    lock_name(shared_root, path)
}

fn keep_state_file(summary: &mut StateReapFamilySummary, path: String, reason: impl Into<String>) {
    summary.kept.push(StateReapKept {
        path,
        reason: reason.into(),
    });
}

fn observe_age(summary: &mut StateReapFamilySummary, age_s: u64) {
    summary.oldest_age_s = Some(summary.oldest_age_s.unwrap_or(0).max(age_s));
}

fn record_state_file_action(
    path: &std::path::Path,
    display_path: String,
    bytes: u64,
    age_s: u64,
    apply: bool,
    summary: &mut StateReapFamilySummary,
) {
    let entry = StateReapEntry {
        path: display_path.clone(),
        bytes,
        age_s,
    };
    if !apply {
        summary.would_delete += 1;
        summary.bytes = summary.bytes.saturating_add(bytes);
        summary.would_delete_entries.push(entry);
        return;
    }
    match std::fs::remove_file(path) {
        Ok(()) => {
            summary.deleted += 1;
            summary.bytes = summary.bytes.saturating_add(bytes);
            summary.deleted_entries.push(entry);
        }
        Err(err) => keep_state_file(summary, display_path, format!("delete failed: {err}")),
    }
}

fn read_state_dir(
    shared_root: &std::path::Path,
    dir: &std::path::Path,
    summary: &mut StateReapFamilySummary,
) -> Option<std::fs::ReadDir> {
    match std::fs::read_dir(dir) {
        Ok(entries) => Some(entries),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            keep_state_file(
                summary,
                state_path(shared_root, dir),
                format!("read directory failed: {err}"),
            );
            None
        }
    }
}

fn reap_mtime_family(
    shared_root: &std::path::Path,
    dir: &std::path::Path,
    retain_days: u64,
    extension: Option<&str>,
    apply: bool,
    summary: &mut StateReapFamilySummary,
) {
    let Some(entries) = read_state_dir(shared_root, dir, summary) else {
        return;
    };
    let now = std::time::SystemTime::now();
    let window_secs = retain_days.saturating_mul(86_400);
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                keep_state_file(
                    summary,
                    state_path(shared_root, dir),
                    format!("read entry failed: {err}"),
                );
                continue;
            }
        };
        let path = entry.path();
        if extension.is_some_and(|wanted| path.extension().and_then(|e| e.to_str()) != Some(wanted))
        {
            continue;
        }
        summary.scanned += 1;
        let display_path = state_path(shared_root, &path);
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(err) => {
                keep_state_file(summary, display_path, format!("metadata failed: {err}"));
                continue;
            }
        };
        let modified = match metadata.modified() {
            Ok(modified) => modified,
            Err(err) => {
                keep_state_file(summary, display_path, format!("metadata failed: {err}"));
                continue;
            }
        };
        let age_s = now.duration_since(modified).unwrap_or_default().as_secs();
        observe_age(summary, age_s);
        if age_s <= window_secs {
            keep_state_file(summary, display_path, "within retention window");
            continue;
        }
        record_state_file_action(&path, display_path, metadata.len(), age_s, apply, summary);
    }
}

fn reap_pr_status_rows(
    shared_root: &Path,
    dir: &Path,
    retain_days: u64,
    apply: bool,
    summary: &mut StateReapFamilySummary,
) {
    let Some(entries) = read_state_dir(shared_root, dir, summary) else {
        return;
    };
    let now = std::time::SystemTime::now();
    let window_secs = retain_days.saturating_mul(86_400);
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        summary.scanned += 1;
        let name = state_path(shared_root, &path);
        let row = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(error) => {
                keep_state_file(summary, name, format!("open failed: {error}"));
                continue;
            }
        };
        let metadata = match row.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                keep_state_file(summary, name, format!("metadata failed: {error}"));
                continue;
            }
        };
        let modified = match metadata.modified() {
            Ok(modified) => modified,
            Err(error) => {
                keep_state_file(summary, name, format!("metadata failed: {error}"));
                continue;
            }
        };
        let age_s = now.duration_since(modified).unwrap_or_default().as_secs();
        observe_age(summary, age_s);
        if age_s <= window_secs {
            keep_state_file(summary, name, "within retention window");
            continue;
        }
        let lock_path = path.with_extension("lock");
        if !apply && !lock_path.exists() {
            match opened_path_matches(&row, &path) {
                Ok(true) => {
                    record_state_file_action(&path, name, metadata.len(), age_s, false, summary)
                }
                Ok(false) => keep_state_file(summary, name, "path replaced"),
                Err(error) => {
                    keep_state_file(summary, name, format!("path revalidation failed: {error}"))
                }
            }
            continue;
        }
        let lock = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(apply)
            .truncate(false)
            .open(&lock_path)
        {
            Ok(file) => file,
            Err(error) => {
                keep_state_file(summary, name, format!("row lock unavailable: {error}"));
                continue;
            }
        };
        if let Err(error) = lock.try_lock() {
            keep_state_file(summary, name, format!("row lock held: {error}"));
            continue;
        }
        match opened_path_matches(&row, &path) {
            Ok(true) => {
                record_state_file_action(&path, name, metadata.len(), age_s, apply, summary)
            }
            Ok(false) => keep_state_file(summary, name, "path replaced"),
            Err(error) => {
                keep_state_file(summary, name, format!("path revalidation failed: {error}"))
            }
        }
        let _ = lock.unlock();
    }
}

fn reap_lock_family(
    shared_root: &std::path::Path,
    dir: &std::path::Path,
    retain_days: u64,
    apply: bool,
    summary: &mut StateReapFamilySummary,
    before_revalidate: &dyn Fn(&std::path::Path),
) {
    let Some(entries) = read_state_dir(shared_root, dir, summary) else {
        return;
    };
    let now = std::time::SystemTime::now();
    let window_secs = retain_days.saturating_mul(86_400);
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                keep_state_file(
                    summary,
                    state_path(shared_root, dir),
                    format!("read entry failed: {err}"),
                );
                continue;
            }
        };
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("lock") {
            continue;
        }
        summary.scanned += 1;
        let name = state_path(shared_root, &path);
        let direct = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata.file_type().is_file(),
            Err(err) => {
                keep_state_file(summary, name, format!("metadata failed: {err}"));
                continue;
            }
        };
        if !direct {
            keep_state_file(summary, name, "not a direct file");
            continue;
        }
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(err) => {
                keep_state_file(summary, name, format!("open failed: {err}"));
                continue;
            }
        };
        let metadata = match file.metadata() {
            Ok(metadata) => metadata,
            Err(err) => {
                keep_state_file(summary, name, format!("metadata failed: {err}"));
                continue;
            }
        };
        let modified = match metadata.modified() {
            Ok(modified) => modified,
            Err(err) => {
                keep_state_file(summary, name, format!("metadata failed: {err}"));
                continue;
            }
        };
        let age_s = now.duration_since(modified).unwrap_or_default().as_secs();
        observe_age(summary, age_s);
        if age_s <= window_secs {
            keep_state_file(summary, name, "within retention window");
            continue;
        }
        if metadata.len() != 0 {
            keep_state_file(summary, name, "nonzero lock file");
            continue;
        }
        if let Err(err) = file.try_lock() {
            let reason = match err {
                std::fs::TryLockError::WouldBlock => "held".to_string(),
                std::fs::TryLockError::Error(err) => format!("flock failed: {err}"),
            };
            keep_state_file(summary, name, reason);
            continue;
        }
        before_revalidate(&path);
        match opened_path_matches(&file, &path) {
            Ok(true) => {
                record_state_file_action(&path, name, metadata.len(), age_s, apply, summary)
            }
            Ok(false) => keep_state_file(summary, name, "path replaced"),
            Err(err) => keep_state_file(summary, name, format!("path revalidation failed: {err}")),
        }
        let _ = file.unlock();
    }
}

fn state_reap_totals(summary: &StateFilesReapSummary) -> StateReapTotals {
    let families = [
        &summary.expired_claims,
        &summary.plan_locks,
        &summary.agent_locks,
        &summary.pr_status_cache,
    ];
    StateReapTotals {
        scanned: families.iter().map(|family| family.scanned).sum(),
        deleted: families.iter().map(|family| family.deleted).sum(),
        would_delete: families.iter().map(|family| family.would_delete).sum(),
        bytes: families.iter().map(|family| family.bytes).sum(),
        oldest_age_s: families
            .iter()
            .filter_map(|family| family.oldest_age_s)
            .max(),
        kept: families.iter().map(|family| family.kept.len()).sum(),
    }
}

struct StateReapRoots {
    claims_root: PathBuf,
    locks_root: PathBuf,
    agents_root: PathBuf,
    state_root: PathBuf,
}

fn reap_state_files_with_roots(
    roots: StateReapRoots,
    config: crate::agents_config::StateReapConfig,
    apply: bool,
) -> StateFilesReapSummary {
    let mut summary = StateFilesReapSummary {
        applied: apply && config.enabled,
        dry_run: !apply,
        ..Default::default()
    };
    if !config.enabled {
        summary.skip_reason = Some("disabled".to_string());
        return summary;
    }
    reap_mtime_family(
        &roots.claims_root,
        &roots.claims_root.join("claims/.expired"),
        config.expired_claims_retain_days,
        None,
        apply,
        &mut summary.expired_claims,
    );
    reap_lock_family(
        &roots.locks_root,
        &roots.locks_root.join("locks"),
        config.locks_retain_days,
        apply,
        &mut summary.plan_locks,
        &|_| {},
    );
    reap_lock_family(
        &roots.agents_root,
        &roots.agents_root.join("locks"),
        config.locks_retain_days,
        apply,
        &mut summary.agent_locks,
        &|_| {},
    );
    let pr_status_dir = roots.state_root.join("cache/pr-status");
    reap_pr_status_rows(
        &roots.state_root,
        &pr_status_dir,
        config.pr_status_cache_retain_days,
        apply,
        &mut summary.pr_status_cache,
    );
    reap_lock_family(
        &roots.state_root,
        &pr_status_dir,
        config.locks_retain_days,
        apply,
        &mut summary.pr_status_cache,
        &|_| {},
    );
    summary.totals = state_reap_totals(&summary);
    summary
}

/// Test/injected-home entry point. Production callers use
/// [`reap_state_files_for_cwd`] so each family follows its canonical resolver.
pub fn reap_state_files(
    home: &AgentsHome,
    config: crate::agents_config::StateReapConfig,
    apply: bool,
) -> StateFilesReapSummary {
    let Some(root) = home.root().parent() else {
        let mut summary = StateFilesReapSummary::default();
        summary.skip_reason = Some("state root unavailable".into());
        return summary;
    };
    reap_state_files_with_roots(
        StateReapRoots {
            claims_root: root.to_path_buf(),
            locks_root: root.to_path_buf(),
            agents_root: root.join("agents"),
            state_root: root.to_path_buf(),
        },
        config,
        apply,
    )
}

/// Production entry point: claims, machine locks, agent locks, and PR cache
/// remain independent roots instead of inheriting `FNO_AGENTS_HOME`'s parent.
pub fn reap_state_files_for_cwd(
    home: &AgentsHome,
    cwd: &std::path::Path,
    config: crate::agents_config::StateReapConfig,
    apply: bool,
) -> StateFilesReapSummary {
    let Some(claims_root) = crate::claims::global_claims_root() else {
        return unavailable_state_reap("claims root unavailable");
    };
    let Some(locks_dir) = crate::agents_config::machine_locks_dir() else {
        return unavailable_state_reap("machine locks root unavailable");
    };
    let Some(state_root) = crate::agents_config::state_dir(cwd) else {
        return unavailable_state_reap("state root unavailable");
    };
    let Some(locks_root) = locks_dir.parent().map(Path::to_path_buf) else {
        return unavailable_state_reap("machine locks root unavailable");
    };
    reap_state_files_with_roots(
        StateReapRoots {
            claims_root,
            locks_root,
            agents_root: home.root().to_path_buf(),
            state_root,
        },
        config,
        apply,
    )
}

fn unavailable_state_reap(reason: &str) -> StateFilesReapSummary {
    StateFilesReapSummary {
        skip_reason: Some(reason.into()),
        ..Default::default()
    }
}

fn row_timestamp(value: Option<&Value>) -> Option<chrono::DateTime<chrono::Utc>> {
    let raw = value?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
}

/// The global ledger's rows, parsed ONCE per sweep. Best-effort: a missing or
/// unreadable ledger answers None and every receipt stands on its row's own
/// fields.
pub(crate) fn ledger_rows(ledger_path: &std::path::Path) -> Option<Vec<Value>> {
    let content = std::fs::read_to_string(ledger_path).ok()?;
    let data: Value = serde_json::from_str(&content).ok()?;
    match data.get("entries").unwrap_or(&data) {
        Value::Array(rows) => Some(rows.to_vec()),
        _ => None,
    }
}

/// The ledger row naming `session_id` in its `sessions`, if any.
pub(crate) fn ledger_entry_in<'a>(rows: &'a [Value], session_id: &str) -> Option<&'a Value> {
    rows.iter().find(|r| {
        r.get("sessions")
            .and_then(Value::as_array)
            .is_some_and(|sessions| sessions.iter().any(|s| s.as_str() == Some(session_id)))
    })
}

/// `$HOME/.fno/ledger.json`, the ledger's default global path. Tests inject
/// their own by building receipts with an explicit `ledger` value instead.
pub(crate) fn default_ledger_path() -> std::path::PathBuf {
    let base = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    base.join(".fno").join("ledger.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stale_state_home(tag: &str) -> (std::path::PathBuf, AgentsHome) {
        let base = std::env::temp_dir().join(format!(
            "fno-expire-stale-state-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = AgentsHome::at(base.join("agents"));
        home.ensure_root().unwrap();
        (base, home)
    }

    fn age_file(path: &std::path::Path, days: u64) {
        let modified = std::time::SystemTime::now()
            .checked_sub(std::time::Duration::from_secs(days * 86_400))
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
    }

    fn run_empty_registry_sweep(home: &AgentsHome, dry_run: bool) -> GcSummary {
        let emitter = EventEmitter::new(home.events_jsonl(), "test");
        run(
            home,
            &emitter,
            900,
            dry_run,
            7,
            &|_| panic!("empty registry must return before graph read"),
            &|_| None,
            &|_| false,
            &|_| crate::daemon::CascadeOutcome::NotApplicable,
            &|| crate::claude_roster::ClaudeAgentsSnapshot::known(Vec::new()),
            &|_| (None, None),
            &|_| None,
        )
    }

    #[test]
    fn row_sweep_does_not_reap_state_files() {
        let (base, home) = stale_state_home("claims");
        let expired = base.join("claims/.expired");
        std::fs::create_dir_all(&expired).unwrap();
        let old = expired.join("old-claim");
        let fresh = expired.join("fresh-claim");
        let future = expired.join("future-claim");
        std::fs::write(&old, b"{}").unwrap();
        std::fs::write(&fresh, b"{}").unwrap();
        std::fs::write(&future, b"{}").unwrap();
        age_file(&old, 40);
        age_file(&fresh, 2);
        std::fs::File::options()
            .write(true)
            .open(&future)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(
                std::time::SystemTime::now() + std::time::Duration::from_secs(86_400),
            ))
            .unwrap();

        let summary = run_empty_registry_sweep(&home, false);

        assert!(old.exists());
        assert!(fresh.exists());
        assert!(future.exists(), "future mtimes saturate to age zero");
        assert_eq!(summary, GcSummary::default());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn state_reap_removes_old_pr_status_json_and_lock_sidecars() {
        let (base, home) = stale_state_home("pr-status");
        let statuses = base.join("cache/pr-status");
        std::fs::create_dir_all(&statuses).unwrap();
        let old = statuses.join("old.json");
        let fresh = statuses.join("fresh.json");
        let lock = statuses.join("old.lock");
        std::fs::write(&old, b"{}").unwrap();
        std::fs::write(&fresh, b"{}").unwrap();
        std::fs::write(&lock, b"").unwrap();
        age_file(&old, 20);
        age_file(&fresh, 2);
        age_file(&lock, 20);

        let summary = reap_state_files(
            &home,
            crate::agents_config::StateReapConfig::default(),
            true,
        );

        assert!(!old.exists());
        assert!(fresh.exists());
        assert!(!lock.exists());
        assert_eq!(summary.pr_status_cache.deleted, 2);
        assert_eq!(summary.pr_status_cache.kept.len(), 1);
        std::fs::remove_dir_all(&base).ok();
    }

    #[cfg(unix)]
    #[test]
    fn expire_stale_state_keeps_and_names_entries_whose_age_cannot_be_read() {
        use std::os::unix::fs::symlink;

        let (base, home) = stale_state_home("unknown-age");
        let expired = base.join("claims/.expired");
        std::fs::create_dir_all(&expired).unwrap();
        let broken = expired.join("broken-claim");
        symlink(expired.join("missing-target"), &broken).unwrap();

        let summary = reap_state_files(
            &home,
            crate::agents_config::StateReapConfig::default(),
            true,
        );

        assert!(broken.symlink_metadata().is_ok());
        assert_eq!(summary.expired_claims.kept.len(), 1);
        assert_eq!(
            summary.expired_claims.kept[0].path,
            "claims/.expired/broken-claim"
        );
        assert!(summary.expired_claims.kept[0]
            .reason
            .contains("metadata failed"));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn expire_stale_state_keeps_fresh_groom_marker_for_staleness_consumer() {
        let (base, home) = stale_state_home("groom");
        let expired = base.join("claims/.expired");
        std::fs::create_dir_all(&expired).unwrap();
        let groom = expired.join("groom:2026-09-08");
        std::fs::write(&groom, b"{}").unwrap();
        age_file(&groom, 2);

        let summary = reap_state_files(
            &home,
            crate::agents_config::StateReapConfig::default(),
            true,
        );

        assert!(groom.exists());
        assert_eq!(summary.expired_claims.deleted, 0);
        assert_eq!(summary.expired_claims.kept.len(), 1);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn expire_stale_state_dry_run_deletes_nothing() {
        let (base, home) = stale_state_home("dry-run");
        let expired = base.join("claims/.expired");
        let statuses = base.join("cache/pr-status");
        std::fs::create_dir_all(&expired).unwrap();
        std::fs::create_dir_all(&statuses).unwrap();
        let claim = expired.join("old-claim");
        let status = statuses.join("old.json");
        std::fs::write(&claim, b"{}").unwrap();
        std::fs::write(&status, b"{}").unwrap();
        age_file(&claim, 40);
        age_file(&status, 20);

        let summary = reap_state_files(
            &home,
            crate::agents_config::StateReapConfig::default(),
            false,
        );

        assert!(claim.exists());
        assert!(status.exists());
        assert_eq!(summary.expired_claims.would_delete, 1);
        assert_eq!(summary.pr_status_cache.would_delete, 1);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn expire_stale_locks_deletes_only_unheld_old_zero_byte_files() {
        let (base, home) = stale_state_home("locks");
        let plan_dir = base.join("locks");
        let agent_dir = base.join("agents/locks");
        let cache_dir = base.join("cache/pr-status");
        for dir in [&plan_dir, &agent_dir, &cache_dir] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let old = plan_dir.join("plan-old.lock");
        let held = agent_dir.join("held.lock");
        let nonzero = cache_dir.join("nonzero.lock");
        std::fs::write(&old, b"").unwrap();
        std::fs::write(&held, b"").unwrap();
        std::fs::write(&nonzero, b"holder").unwrap();
        for path in [&old, &held, &nonzero] {
            age_file(path, 8);
        }
        let held_file = std::fs::File::options()
            .read(true)
            .write(true)
            .open(&held)
            .unwrap();
        held_file.try_lock().unwrap();

        let summary = reap_state_files(
            &home,
            crate::agents_config::StateReapConfig::default(),
            true,
        );

        assert!(!old.exists());
        assert!(held.exists());
        assert!(nonzero.exists());
        assert!(summary
            .plan_locks
            .deleted_entries
            .iter()
            .any(|entry| entry.path == "locks/plan-old.lock"));
        assert!(summary
            .agent_locks
            .kept
            .iter()
            .any(|entry| entry.path == "agents/locks/held.lock" && entry.reason.contains("held")));
        assert!(summary
            .pr_status_cache
            .kept
            .iter()
            .any(|entry| entry.path == "cache/pr-status/nonzero.lock"
                && entry.reason.contains("nonzero")));
        held_file.unlock().unwrap();
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn expire_stale_locks_keeps_a_path_replaced_before_unlink() {
        let (base, home) = stale_state_home("lock-race");
        let lock_dir = base.join("locks");
        std::fs::create_dir_all(&lock_dir).unwrap();
        let path = lock_dir.join("plan-race.lock");
        std::fs::write(&path, b"").unwrap();
        age_file(&path, 8);

        let mut summary = StateReapFamilySummary::default();
        reap_lock_family(&base, &lock_dir, 7, true, &mut summary, &|candidate| {
            if candidate == path {
                std::fs::remove_file(candidate).unwrap();
                std::fs::write(candidate, b"").unwrap();
            }
        });

        assert!(path.exists());
        assert!(summary
            .kept
            .iter()
            .any(|entry| entry.path == "locks/plan-race.lock"
                && entry.reason.contains("path replaced")));
        std::fs::remove_dir_all(&base).ok();
        drop(home);
    }

    #[test]
    fn expire_stale_locks_dry_run_deletes_nothing() {
        let (base, home) = stale_state_home("lock-dry-run");
        let path = base.join("locks/plan-old.lock");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"").unwrap();
        age_file(&path, 8);

        let summary = reap_state_files(
            &home,
            crate::agents_config::StateReapConfig::default(),
            false,
        );

        assert!(path.exists());
        assert_eq!(summary.plan_locks.deleted, 0);
        assert_eq!(summary.plan_locks.would_delete, 1);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn state_files_only_dry_run_enumerates_all_families_without_deleting() {
        let (base, home) = stale_state_home("state-files-dry-run");
        let claim = base.join("claims/.expired/old-claim");
        let plan_lock = base.join("locks/plan-old.lock");
        let quota_lock = base.join("locks/github-graphql-quota.lock");
        let agent_lock = base.join("agents/locks/worker-old.lock");
        let pr_status = base.join("cache/pr-status/42.json");
        for path in [&claim, &plan_lock, &quota_lock, &agent_lock, &pr_status] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"").unwrap();
            age_file(path, 40);
        }

        let summary = reap_state_files(
            &home,
            crate::agents_config::StateReapConfig {
                enabled: true,
                locks_retain_days: 7,
                expired_claims_retain_days: 30,
                pr_status_cache_retain_days: 14,
            },
            false,
        );

        assert!(claim.exists());
        assert!(plan_lock.exists());
        assert!(quota_lock.exists());
        assert!(agent_lock.exists());
        assert!(pr_status.exists());
        assert_eq!(summary.expired_claims.would_delete, 1);
        assert_eq!(summary.plan_locks.would_delete, 2);
        assert_eq!(summary.agent_locks.would_delete, 1);
        assert_eq!(summary.pr_status_cache.would_delete, 1);
        assert_eq!(summary.totals.would_delete, 5);
        assert_eq!(
            summary.totals.would_delete,
            summary.expired_claims.would_delete
                + summary.plan_locks.would_delete
                + summary.agent_locks.would_delete
                + summary.pr_status_cache.would_delete
        );
        assert!(!summary.applied);
        assert!(summary.dry_run);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn state_files_only_apply_uses_config_and_never_retires_registry_rows() {
        let (base, home) = stale_state_home("state-files-apply");
        let claim = base.join("claims/.expired/old-claim");
        std::fs::create_dir_all(claim.parent().unwrap()).unwrap();
        std::fs::write(&claim, b"claim").unwrap();
        age_file(&claim, 40);
        let registry =
            br#"{"entries":[{"name":"live-worker","created_at":"2026-09-09T00:00:00Z"}]}"#;
        std::fs::write(home.registry_json(), registry).unwrap();

        let summary = reap_state_files(
            &home,
            crate::agents_config::StateReapConfig::default(),
            true,
        );

        assert!(!claim.exists());
        assert_eq!(summary.expired_claims.deleted, 1);
        assert_eq!(summary.totals.deleted, 1);
        assert_eq!(std::fs::read(home.registry_json()).unwrap(), registry);
        assert!(summary.applied);
        assert!(!summary.dry_run);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn state_files_only_disabled_is_an_exact_named_skip() {
        let (base, home) = stale_state_home("state-files-disabled");
        let claim = base.join("claims/.expired/old-claim");
        std::fs::create_dir_all(claim.parent().unwrap()).unwrap();
        std::fs::write(&claim, b"claim").unwrap();
        age_file(&claim, 40);
        let mut config = crate::agents_config::StateReapConfig::default();
        config.enabled = false;

        let summary = reap_state_files(&home, config, true);

        assert!(claim.exists());
        assert_eq!(summary.skip_reason.as_deref(), Some("disabled"));
        assert_eq!(summary.totals.deleted, 0);
        assert_eq!(summary.totals.would_delete, 0);
        std::fs::remove_dir_all(&base).ok();
    }

    /// A one-worker roster in the confirmed live shape (the shape the
    /// claude_roster parse test accepts).
    const ONE_WORKER_ROSTER: &str = r#"{
  "proto": 1,
  "supervisorPid": 4242,
  "updatedAt": 1751049130000,
  "workers": {
    "ee99ff00": {
      "pid": 5002,
      "sessionId": "ee99ff00-7777-8888-9999-aaaabbbbcccc",
      "ptySock": "/tmp/cc-daemon-501/deadbeef/pty/ee99ff00.pty.sock",
      "startedAt": 1751049050000,
      "attempt": 2,
      "cwd": "/Users/x/code/other",
      "dispatch": {"source": "fleet"}
    }
  }
}"#;

    fn roster_file(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("fno-roster-lists-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("roster.json");
        std::fs::write(&path, ONE_WORKER_ROSTER).unwrap();
        path
    }

    #[test]
    fn roster_lists_by_short_id_session_id_or_not_at_all() {
        let path = roster_file("listed");
        assert_eq!(
            roster_lists_in(&path, "ee99ff00", None),
            Some(true),
            "listed by short id"
        );
        assert_eq!(
            roster_lists_in(&path, "ee99ff00-7777-8888-9999-aaaabbbbcccc", None),
            Some(true),
            "listed by session id"
        );
        assert_eq!(
            roster_lists_in(&path, "deadbeef", None),
            Some(false),
            "an unknown session is not listed"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn torn_roster_read_is_unknown_not_gone() {
        let dir = std::env::temp_dir().join(format!("fno-roster-torn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("roster.json")).unwrap();
        assert_eq!(
            roster_lists_in(&dir.join("roster.json"), "ee99ff00", None),
            None,
            "a torn read is not an exited proof"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // The cross-check runs even when the reverse join answers: a name
    // resolving a DIFFERENT node than sessions[] holds the row, it does not
    // retire on the join's answer alone.
    #[test]
    fn a_name_contradicting_the_session_join_holds_the_row() {
        use crate::gc::KeepReason;
        use std::collections::HashMap;
        let mut e =
            crate::state::RegistryEntry::new(Some("sid-77".into()), crate::state::Lineage::none());
        e.name = "target-N2".into();
        e.origin = Some("spawn".into());
        let graph = GraphRead {
            index: HashMap::from([(
                crate::graph_store::work_state_key("sid-77"),
                vec![("N1".to_string(), "review".to_string())],
            )]),
            statuses: HashMap::from([
                ("N1".to_string(), "done".to_string()),
                ("N2".to_string(), "open".to_string()),
            ]),
            pr_state: HashMap::from([("N1".to_string(), (Some("merged".into()), 0))]),
            ..Default::default()
        };
        let verdict = provenance_verdict(&e, "sid-77", &graph, None);
        assert_eq!(
            verdict.hold,
            Some(KeepReason::NodeConflict {
                a: "name".into(),
                b: "N2".into()
            }),
            "the contradicting witness holds the row"
        );
        assert!(
            matches!(verdict.work, WorkState::NoProvenance),
            "a conflict leaves no work verdict to retire on"
        );
    }
}
