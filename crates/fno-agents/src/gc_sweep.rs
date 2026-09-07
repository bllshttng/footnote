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
//! Retirement never removes the session from its harness's
//! store, and never deletes a branch. The node's `sessions[]` row and the
//! transcript survive the retirement, so `fno agents resume` still opens the
//! session afterwards.

use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::{json, Value};

use crate::events::EventEmitter;
use crate::gc::{
    gc_decide, row_handle, transcript_age_s, tree_action, GcAction, GcRow, KeepReason, TreeAction,
};
use crate::graph_store::{self, WorkState};
use crate::paths::AgentsHome;
use crate::receipt::{build_reap_receipt, write_reap_receipt, ReapReceipt};
use crate::state;

pub(crate) use crate::daemon::HarnessStoreIndex;

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
    pub kept_operator: Vec<String>,
    pub kept_crowned: Vec<String>,
    /// `(id, origin)`: origin is not `spawn` (adopted, unknown spelling), so
    /// a sweep never removes it - only a row fno itself spawned retires.
    pub kept_not_spawn: Vec<(String, String)>,
    /// Named in no node's `sessions[]`: no provenance, no work-done verdict.
    pub kept_no_provenance: Vec<String>,
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

/// The graph read that feeds a sweep: the entries (working graph plus
/// archive), the reverse-join index over them, and the open-do map (`session
/// -> nodes carrying an OPEN do row for it`).
#[derive(Debug, Default, Clone)]
pub(crate) struct GraphRead {
    pub index: HashMap<String, Vec<(String, String)>>,
    pub open_do: HashMap<String, Vec<String>>,
}

/// One row the pass decided to retire, with everything the write tail needs.
struct RetireOrder {
    id: String,
    basis: String,
    created_at: String,
    tree: TreeAction,
    worktree: Option<String>,
}

/// The state root's graph file: the one `read_graph_entries` reads (plus the
/// advisory archive) and the one the settle writes under the lock.
pub(crate) fn graph_path(home: &AgentsHome) -> PathBuf {
    let state_root = home.root().parent().unwrap_or(home.root());
    state_root.join("graph.json")
}

/// Read the working graph plus the archive and build the reverse-join index
/// and the open-do map. The archive is advisory (a read failure contributes
/// nothing); the WORKING graph failing to parse is `None` and the sweep keeps
/// every row as `graph unreadable`. A missing graph file is an empty graph
/// (every row reads `no provenance`), matching the Python read seam.
pub(crate) fn read_graph_entries(home: &AgentsHome) -> Option<GraphRead> {
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
    let index = graph_store::sessions_index(&entries);
    let mut open_do: HashMap<String, Vec<String>> = HashMap::new();
    for entry in &entries {
        let Some(node_id) = graph_store::entry_id(entry) else {
            continue;
        };
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
        }
    }
    Some(GraphRead { index, open_do })
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

/// Stop a retiring row's held process from a sync caller. The stop is async,
/// so it runs on a dedicated thread with a one-shot current-thread runtime:
/// `Handle::block_on` on the caller's own thread panics inside an ambient
/// runtime (the CLI verb runs under `main`'s `block_on`, the daemon's tick
/// under `spawn_blocking`), and a fresh thread is legal in both. A runtime
/// that cannot be built fails closed: the row keeps under `stop_refused`.
pub(crate) fn stop_row_process(home: &AgentsHome, e: &state::RegistryEntry) -> bool {
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

/// The one retirement pass. Every I/O seam (`read_graph`, `store_matches`,
/// `stop_confirmed`, `tree_probe`, `prune_tree`) is injected so a test
/// stages the world; production wiring is [`crate::gc::gc_sweep`] /
/// [`crate::gc::gc_sweep_dry_run`].
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
    tree_probe: &dyn Fn(&state::RegistryEntry) -> (Option<bool>, Option<bool>),
    prune_tree: &dyn Fn(&state::RegistryEntry),
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
        let work = graph_store::work_state(&graph.index, sid);
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
        let age = transcript_age_s(store_matches(e).as_deref(), now);
        let owns_worktree = !e.is_one_shot_ask() && crate::daemon::is_linked_worktree(&e.cwd);
        let row = GcRow {
            origin: e.origin.clone(),
            crowned: e.crown_level.is_some(),
            work,
            transcript_age_s: age,
            owns_worktree,
            worktree_clean: None,
            branch_merged: None,
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
                // GraphUnreadable / OpenDoRow are decided above, before the
                // policy ran; they cannot arrive here.
                _ => {}
            }
            continue;
        }
        // A retiring row first confirms its process is stopped: a refusal
        // keeps the row this tick and names the refusal. DRY-RUN never stops
        // anything - a rehearsal that killed the worker it rehearsed
        // retiring would be the destructive run wearing a dry flag.
        let stopped = if dry_run { true } else { stop_confirmed(e) };
        if !stopped {
            summary
                .stop_refused
                .push((id, "the stop did not confirm; row kept for retry".into()));
            continue;
        }
        if !stage_reap_receipt(
            e,
            &id,
            ledger.as_deref(),
            &mut receipts,
            &mut summary.kept_no_receipt,
        ) {
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
        let basis = match &probed.work {
            WorkState::AllDone { nodes } => {
                format!("every named node done: {}", nodes.join(", "))
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

    // Persist every receipt BEFORE the write drops its row: the ordering IS
    // the losslessness. A receipt that will not write holds its row for the
    // next sweep instead.
    to_retire.retain(|name, _| {
        let Some(receipt) = receipts.get(name) else {
            summary
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
                summary
                    .kept_no_receipt
                    .push((id, format!("receipt did not persist: {err}")));
                false
            }
        }
    });
    // Names actually removed under the lock (identity still matched), so the
    // emit + summary report only what really happened.
    let mut retired_names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let write = state::update_registry(&home.registry_json(), |r| {
        r.entries.retain(|e| {
            let Some(order) = to_retire.get(&e.name) else {
                return true;
            };
            if order.created_at != e.created_at {
                return true; // a replacement session owns this name now
            }
            retired_names.insert(e.name.clone());
            false
        });
    });
    match write {
        Ok(()) => {
            for e in &registry.entries {
                let Some(order) = to_retire.get(&e.name) else {
                    continue;
                };
                if !retired_names.contains(&e.name) {
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
                summary
                    .retired
                    .push((order.id.clone(), order.basis.clone()));
                if order.tree == TreeAction::Prune {
                    if let Some(path) = &order.worktree {
                        // The same door a human removal walks (production:
                        // gate + merge check + `git worktree remove`; the
                        // branch survives).
                        prune_tree(e);
                        summary.pruned.push((order.id.clone(), path.clone()));
                    }
                }
            }
        }
        Err(err) => {
            let _ = emitter.emit(
                "daemon_recovery_error",
                &json!({"op": "gc_sweep", "error": err.to_string()}),
            );
            // Nothing was removed; report no retirements (no event/disk
            // divergence).
            summary.retired.clear();
            summary.pruned.clear();
        }
    }
    summary
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
    for entry in entries.flatten() {
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
                serde_json::from_slice::<Value>(&raw).map_err(|e| format!("receipt malformed: {e}"))
            });
        let reaped = match read {
            Ok(value) => row_timestamp(value.get("reaped_at")),
            Err(reason) => {
                summary.kept_receipts.push((name, reason));
                continue;
            }
        };
        let Some(reaped) = reaped else {
            summary.kept_receipts.push((
                name,
                "reaped_at missing or unparseable; a failed read is not evidence of age"
                    .to_string(),
            ));
            continue;
        };
        let age_secs = (now - reaped).num_seconds().max(0) as u64;
        if age_secs > window_secs {
            match std::fs::remove_file(&path) {
                Ok(()) => summary.expired_receipts.push(name),
                Err(err) => summary
                    .kept_receipts
                    .push((name, format!("expiry failed: {err}"))),
            }
        }
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

/// The receipt gate at the retirement arm: stages the receipt before the row
/// can drop; a row whose receipt cannot be staged is held and named in
/// `kept_no_receipt`.
fn stage_reap_receipt(
    e: &state::RegistryEntry,
    id: &str,
    ledger_rows: Option<&[Value]>,
    receipts: &mut std::collections::BTreeMap<String, ReapReceipt>,
    kept_no_receipt: &mut Vec<(String, String)>,
) -> bool {
    let ledger = ledger_rows
        .and_then(|rows| ledger_entry_in(rows, e.harness_session_id.as_deref().unwrap_or("")));
    match build_reap_receipt(e, ledger) {
        Ok(receipt) => {
            receipts.insert(e.name.clone(), receipt);
            true
        }
        Err(reason) => {
            kept_no_receipt.push((id.to_string(), reason));
            false
        }
    }
}

/// `$HOME/.fno/ledger.json`, the ledger's default global path. Tests inject
/// their own by building receipts with an explicit `ledger` value instead.
fn default_ledger_path() -> std::path::PathBuf {
    let base = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    base.join(".fno").join("ledger.json")
}
