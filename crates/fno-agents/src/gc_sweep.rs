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
//! flight keeps under `open do row on done node`. That stranded population
//! (an open do row is what holds its node out of this settle's own gate) has
//! its own lane: `fno backlog maintain` detects it in Python, where the
//! transcript resolver lives, and reaps a row only after the prover proves
//! the session gone. Widen THIS gate never; extend that lane instead.
//!
//! Retirement removes the session from its harness's ACTIVE surface only
//! (the agent list, the session index); the native history is never deleted,
//! and neither is a branch. The node's `sessions[]` row and the transcript
//! survive the retirement, so `fno agents resume` still opens the session
//! afterwards.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use crate::events::EventEmitter;
use crate::gc::{gc_decide, row_handle, tree_action, GcAction, GcRow, KeepReason, TreeAction};
use crate::graph_store::{self, WorkState};
use crate::node_route;
use crate::paths::AgentsHome;

/// (x-1b90 change 3) How long a row has sat unresolved: now minus
/// `last_message_at`, else `created_at`; a stamp that cannot parse names no
/// age (0 keeps the line shape without inventing a number).
fn unresolved_hold_secs(e: &state::RegistryEntry, now: i64) -> i64 {
    let parsed = e
        .last_message_at
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(state::rfc3339_like_to_secs)
        .or_else(|| state::rfc3339_like_to_secs(&e.created_at));
    parsed.map_or(0, |at| (now - at as i64).max(0))
}
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
    /// until that child is gone. A parent whose own harness reports a
    /// terminal state is not held: the lineage guard exists to keep a
    /// running parent's surface alive for its children, and a terminal
    /// parent has none.
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
    /// `(id, node, status, reader)` (x-2774 change 4): a named node is not
    /// done; the first open one, and the provenance source that resolved it,
    /// so a sessions-join keep is distinguishable from a name-pattern keep.
    pub kept_open_work: Vec<(String, String, String, String)>,
    /// `(id, age_s)`: the transcript was written inside the grace window.
    pub kept_active: Vec<(String, i64)>,
    /// The transcript could not be resolved through the row's own store.
    /// (x-1b90 change 3) Rows of `{ id, held_s, nodes_done }`: the hold
    /// names its age, and an old hold on done work asks for a decision.
    pub kept_transcript_unresolved: Vec<UnresolvedHold>,
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
    /// The registry file could not be read this pass. Never a retirement on
    /// a failed read; the tick names this instead of a quiet no_rows.
    pub registry_unreadable: bool,
    /// One per held row (x-e3cc): its clock. The text buckets above stay
    /// exactly as they were; `holds` is the read-side projection that gives
    /// a keep an age, a basis, and an escalation flag. It is a projection
    /// OVER the kept_* buckets, never a bucket itself: kept_total must not
    /// count it.
    pub holds: Vec<Hold>,
    /// The escalation threshold `mark_escalated` stamped with, so the
    /// renderer prints the same number the verb enforced. `None` until
    /// stamped; an unstamped summary renders no escalation.
    pub hold_escalate_after_s: Option<u64>,
    /// Rulings the sweep could not apply (x-e3cc): a release named a row
    /// whose current hold no longer matched the one it captured. The row
    /// keeps under its real hold; the reason rides here and in the JSON.
    pub release_refused: Vec<String>,
}

/// The ruling a `reap --release <row>` carries into the sweep (x-e3cc): the
/// row's classified hold, captured when the verb classified it. The sweep
/// lifts one gate on the matching row only, and only while the row's
/// current hold still equals the captured one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Release {
    /// The row handle the release names (the `id` every bucket carries).
    pub handle: String,
    /// The hold reason the classifier answered for the row.
    pub reason: String,
    /// The hold detail at classification time.
    pub detail: String,
}

impl Release {
    /// The stop family (x-e3cc): `needs live stop` is the dry-run spelling
    /// and `stop refused` the real-run spelling of the same missing-death-
    /// evidence fact, so the two reasons answer for each other. Their
    /// details are mode-dependent wording and are not compared.
    fn is_stop_family(&self) -> bool {
        self.reason == "needs live stop" || self.reason == "stop refused"
    }

    /// Does this release answer for a row currently held with `reason` and
    /// `detail`?
    fn matches(&self, reason: &str, detail: &str) -> bool {
        if self.is_stop_family() && (reason == "needs live stop" || reason == "stop refused") {
            return true;
        }
        self.reason == reason && self.detail == detail
    }
}

/// One held row with its clock (x-e3cc): a correct hold with no age is
/// indistinguishable from a reader that never ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Hold {
    /// The row handle (`row_handle`).
    pub id: String,
    /// `KeepReason::as_str`, or "needs live stop" / "stop refused" for the
    /// two buckets KeepReason has no variant for.
    pub reason: &'static str,
    /// The bucket's own detail; an open do row adds its settle blocker.
    pub detail: String,
    /// Transcript-quiet seconds when the age seam answered, else seconds
    /// since the row's `created_at`, else `None`.
    pub age_s: Option<i64>,
    /// "transcript quiet" | "row created" | "unmeasured"
    pub age_basis: &'static str,
    /// Set by `mark_escalated`: age_s past the configured threshold. An
    /// unmeasured hold never escalates.
    pub escalated: bool,
}

impl GcSummary {
    /// Every `kept_*` bucket summed: the rows the pass judged but did not
    /// retire. Zero alongside an empty `retired` means the pass classified
    /// no row at all (the retire tick's `no_rows` skip reason).
    pub fn kept_total(&self) -> usize {
        self.kept_shared_tree.len()
            + self.kept_live_descendants.len()
            + self.kept_operator.len()
            + self.kept_crowned.len()
            + self.kept_not_spawn.len()
            + self.kept_no_provenance.len()
            + self.kept_node_conflict.len()
            + self.kept_pr_contradicts.len()
            + self.kept_planning_unclosed.len()
            + self.kept_open_work.len()
            + self.kept_active.len()
            + self.kept_transcript_unresolved.len()
            + self.kept_graph_unreadable.len()
            + self.kept_open_do_row.len()
            + self.kept_dirty.len()
            + self.kept_unmerged.len()
            + self.kept_unprobed.len()
            + self.kept_no_receipt.len()
            + self.kept_receipts.len()
    }

    /// Stamp every hold's escalation flag against the configured threshold
    /// (x-e3cc). Called once per verb run, after the sweep filled the
    /// buckets; the sweep itself stays clock-free so a dry run and a real
    /// run carry the same hold rows.
    pub fn mark_escalated(&mut self, after: Duration) {
        self.hold_escalate_after_s = Some(after.as_secs());
        let after = after.as_secs() as i64;
        for h in &mut self.holds {
            h.escalated = h.age_s.is_some_and(|a| a >= after);
        }
    }
}

/// One state file selected for deletion by the shared age policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StateReapEntry {
    pub path: String,
    pub bytes: u64,
    pub age_s: u64,
}

/// (x-1b90 change 3) One transcript-unresolved hold: the row, how long it
/// has sat unresolved (now minus `last_message_at`, else `created_at`), and
/// whether every node the row names reads done. The hold is right; what it
/// lacked was a clock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnresolvedHold {
    pub id: String,
    pub held_s: i64,
    pub nodes_done: bool,
}

/// (x-1b90 change 3) When an unresolved hold is this old AND the row's work
/// is all done, the render asks for a decision: `fno agents rm <name>` - an
/// rm that, since change 1, proves the death it prints.
pub(crate) const UNRESOLVED_HOLD_DECIDE_S: i64 = 6 * 3600;

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
    /// Node id -> (merge_status, additional_prs total, additional_prs still
    /// open by recorded state) (x-5a62, x-2774 change 7). The confirm step
    /// reads positive PR-state evidence from it; a missing merge_status is
    /// recorded as unrecorded, never asserted unmerged, and an additional PR
    /// whose state is unrecorded still counts as open.
    pub pr_state: HashMap<String, (Option<String>, usize, usize)>,
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
    /// The session-shaped release (x-2774) that let an OPEN-work row
    /// retire: terminal state, live peer, parked node, or recorded merge.
    /// The obligation re-checks yield to it - the released session's own
    /// open do row is the stale record of work that moved on, not a live
    /// assignment.
    pub(crate) released: bool,
    /// A `reap --release` ruling applied to this row (x-e3cc): the event
    /// names the release as the remover.
    pub(crate) via_release: bool,
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
    let mut pr_state: HashMap<String, (Option<String>, usize, usize)> = HashMap::new();
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
        let additional = entry
            .get("additional_prs")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let additional_open = additional
            .iter()
            .filter(|extra| additional_pr_recorded_open(extra))
            .count();
        pr_state.insert(
            node_id.to_string(),
            (
                entry
                    .get("merge_status")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                additional.len(),
                additional_open,
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

/// Whether an `additional_prs` entry is still open by RECORDED state alone
/// (x-2774 change 7). Only an entry whose own `merge_status` reads `merged`
/// has settled; an entry with no recorded state is still open - absence is
/// never read as merged, the same fail-closed direction the node-level
/// confirm takes. Nothing here queries a live tracker: recording the state
/// at merge time is `fno do pr merge`'s job (`_sync_graph_merge_status`).
pub(crate) fn additional_pr_recorded_open(extra: &Value) -> bool {
    extra.get("merge_status").and_then(Value::as_str) != Some("merged")
}

/// Every open do row sitting on a settled node. Every clause is a positive
/// marker: `status == "done"`; `merge_status == "merged"`, a field written
/// only when a caller resolved MERGED from `gh`, so its absence has two
/// explanations and neither is asserted here; and no `additional_prs` entry
/// still open by recorded state - presence alone never holds the row, an
/// entry whose recorded merge state says merged does not either, and an
/// UNRECORDED entry does (x-2774 change 7).
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
            .is_some_and(|a| a.iter().any(|extra| additional_pr_recorded_open(extra)));
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
                std::thread::sleep(std::time::Duration::from_millis(settle_backoff_ms(attempt)));
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

/// Full-jitter exponential delay before settle retry attempt `attempt + 1`,
/// the shape x-1601 landed on the Python side (`_tx_backoff_secs`): sweepers
/// are correlated by construction, so the flat 250 ms re-lined every loser
/// up at the same instant. Uniform in [0, min(cap, base << attempt)].
const SETTLE_BACKOFF_BASE_MS: u64 = 250;
const SETTLE_BACKOFF_CAP_MS: u64 = 4_000;

fn settle_backoff_ms(attempt: usize) -> u64 {
    let bound = SETTLE_BACKOFF_CAP_MS.min(SETTLE_BACKOFF_BASE_MS << attempt);
    let mut buf = [0u8; 8];
    // A failed entropy draw sleeps 0: the immediate retry this replaces,
    // never a panic in a sweep thread.
    if getrandom::fill(&mut buf).is_err() {
        return 0;
    }
    u64::from_le_bytes(buf) % (bound + 1)
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

/// Fill ONE open do row with `ended_by: "reap-release"` (x-e3cc): the
/// release verb's narrowed settle. Fill-if-absent over a fresh read, the
/// same blocker gate the batch settle applies (a node whose additional PRs
/// are not recorded merged is not fillable), and the same bounded retry
/// before the write refuses. `Ok(false)` = the pair is not fillable: the
/// row was already closed, or the node still carries a blocker - the
/// caller keeps the row either way.
fn settle_one_do_row(home: &AgentsHome, node: &str, session_id: &str) -> Result<bool, String> {
    let path = graph_path(home);
    const ATTEMPTS: usize = 5;
    for attempt in 0..ATTEMPTS {
        let base = graph_store::file_content_version(&path);
        let mut entries = match graph_store::read_defaulted(&path, false) {
            Ok(e) => e,
            Err(err) => return Err(format!("graph unreadable: {err}")),
        };
        // The same eligibility the batch settle applies: the pair must sit
        // in the batch's own stale set.
        let eligible = stale_open_do_rows(&entries)
            .iter()
            .any(|r| r.node == node && r.session_id.eq_ignore_ascii_case(session_id));
        if !eligible {
            return Ok(false);
        }
        let now = crate::daemon::now_rfc3339_like();
        let mut filled = false;
        if let Some(entry) = entries
            .iter_mut()
            .find(|e| graph_store::entry_id(e) == Some(node))
        {
            if let Some(sessions) = entry.get_mut("sessions").and_then(Value::as_array_mut) {
                for session in sessions.iter_mut() {
                    let matches = graph_store::is_open_do_row(session)
                        && session.get("session_id").and_then(Value::as_str) == Some(session_id);
                    if matches {
                        if let Some(obj) = session.as_object_mut() {
                            obj.entry("ended_at".to_string())
                                .or_insert_with(|| Value::String(now.clone()));
                            obj.entry("ended_by".to_string())
                                .or_insert_with(|| Value::String("reap-release".into()));
                            filled = true;
                        }
                    }
                }
            }
        }
        if !filled {
            return Ok(false);
        }
        let outcome = graph_store::locked_mutate(
            &path,
            graph_store::MutateInput {
                entries,
                canonical_path: None,
                base_version: Some(base),
                plan_rungs: None,
            },
            graph_store::DEFAULT_LOCK_TIMEOUT,
        );
        match outcome {
            Ok(_) => return Ok(true),
            Err(
                err
                @ (graph_store::StoreError::Conflict | graph_store::StoreError::LockTimeout(..)),
            ) if attempt + 1 < ATTEMPTS => {
                std::thread::sleep(std::time::Duration::from_millis(250));
                let _ = err;
            }
            Err(
                err
                @ (graph_store::StoreError::Conflict | graph_store::StoreError::LockTimeout(..)),
            ) => {
                return Err(format!(
                    "settle write refused: {err} (after {ATTEMPTS} attempts)"
                ))
            }
            Err(err) => return Err(format!("settle write refused: {err}")),
        }
    }
    unreachable!("every loop arm returns")
}

/// The retire-basis prefix a release rides (x-e3cc): `released <reason>
/// held <age>: <detail>; `. The age uses the same human clock the hold
/// lines render.
fn release_basis_prefix(reason: &str, age_s: Option<i64>, detail: &str) -> String {
    let age = age_s
        .map(crate::reap_render::human_duration)
        .unwrap_or_else(|| "unmeasured".to_string());
    format!("released {reason} held {age}: {detail}; ")
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
/// never be the proof here. `blocked` is NOT terminal: the session is
/// waiting for input, so it holds.
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

/// The three witnesses that say a worker finished. Any one suffices: a
/// transcript older than the grace window, a held pid that answers ESRCH,
/// or a terminal state on the harness roster. Liveness alone never vetoes:
/// a terminal state outranks a fresh transcript. The merge reaper passes
/// the roster's death evidence as `terminal`; the sweep passes `None` and
/// keeps its own classify-to-apply gap clause on top, because only the
/// sweep holds a classification age to compare against.
pub(crate) fn worker_finished(
    e: &state::RegistryEntry,
    age_s: Option<i64>,
    grace_secs: i64,
    terminal: Option<&str>,
) -> bool {
    if matches!(age_s, Some(a) if a > grace_secs) {
        return true;
    }
    if e.pid.is_some_and(crate::daemon::pid_is_gone) {
        return true;
    }
    terminal.is_some()
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

/// The stop detail for the arms whose injected seam answers a bare bool
/// (x-9485 change 1): which arm ran, the session, and the registered pid.
/// The pane arm carries its own measurement; this fills the gap so a
/// confirmed-removed effect names the pid the outcome is about, and the
/// checked-in probe (`scripts/probes/reap-receipt-stop-probe.py`) verifies
/// that pid reads gone.
fn stop_row_detail(e: &state::RegistryEntry) -> Option<String> {
    if e.harness_name() == "claude" {
        let sid = e.harness_session_id.as_deref()?;
        Some(format!("claude stop ran; session {sid}"))
    } else {
        let pid = e.pid.map(|p| format!("; pid {p}")).unwrap_or_default();
        Some(format!("worker socket stop ran{pid}"))
    }
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
    /// The Open node whose RECORDED `merge_status` reads `merged`
    /// (x-2774 change 6): the status field can lag the merge by minutes
    /// when reconcile is slow, and the merge evidence is already in the
    /// same graph read. `Some(node)` releases the open-work shield the way
    /// a terminal session state does; absence keeps the row, because a
    /// conservative hold is the right failure and a wrong reap is not.
    pub merged_but_open: Option<String>,
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
            // Both witnesses ride the hold (x-e3cc): "sources disagree" naming
            // only the dissenting node made a reader re-derive the first half.
            a: format!(
                "{} {}",
                route.source.map(|s| s.as_str()).unwrap_or("sessions"),
                route.node.clone().unwrap_or_default()
            ),
            b: format!("{} {}", src.as_str(), node),
        });
    let mut merge_note: Vec<String> = Vec::new();
    let mut merged_but_open: Option<String> = None;
    match &work {
        WorkState::AllDone { nodes } => {
            for node in nodes {
                let (merge_status, total, open) =
                    graph.pr_state.get(node).cloned().unwrap_or((None, 0, 0));
                if open > 0 {
                    hold = Some(KeepReason::PrStateContradicts {
                        node: node.clone(),
                        detail: format!("additional_prs: {open} of {total} not recorded merged"),
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
        // The merge-lag window (x-2774 change 6): the pr_state read used to
        // be fenced inside the AllDone arm, so the merge evidence already
        // loaded for every node was discarded on the one branch that needs
        // it. Recorded merged is positive evidence the work shipped; only
        // Some("merged") counts, everything else keeps the row.
        WorkState::Open { node, .. } => {
            let (merge_status, _total, _open) =
                graph.pr_state.get(node).cloned().unwrap_or((None, 0, 0));
            if merge_status.as_deref() == Some("merged") {
                merged_but_open = Some(node.clone());
            }
        }
        WorkState::NoProvenance => {}
    }
    ProvenanceVerdict {
        work,
        route,
        hold,
        merge_note,
        merged_but_open,
    }
}

/// The hold clock (x-e3cc): the staged transcript age when the seam
/// answered, else seconds since the row's `created_at`, else unmeasured -
/// and an unmeasured hold never escalates, because a wrong number is not a
/// clock.
fn hold_clock(age: Option<i64>, created_at: &str, now: i64) -> (Option<i64>, &'static str) {
    match age {
        Some(a) => (Some(a), "transcript quiet"),
        None => match crate::tick_ledger::parse_rfc3339_unix(created_at) {
            Some(created) => (Some((now - created as i64).max(0)), "row created"),
            None => (None, "unmeasured"),
        },
    }
}

/// The settle blocker an open-do-row hold names (x-e3cc): the same pr_state
/// read the confirm step makes, so the hold line carries the why the settle
/// refused, not only the node.
fn settle_blocker_detail(graph: &GraphRead, node: &str) -> String {
    let (merge_status, total, open) = graph.pr_state.get(node).cloned().unwrap_or((None, 0, 0));
    if open > 0 {
        format!("additional_prs: {open} of {total} not recorded merged")
    } else if let Some(status) = merge_status.filter(|m| m != "merged") {
        format!("merge_status: {status}")
    } else {
        "merge_status: unrecorded".to_string()
    }
}

/// The one retirement pass. Every I/O seam (`read_graph`, `store_matches`,
/// `age_many`, `stop_confirmed`, `tree_probe`, `prune_tree`) is injected so a
/// test stages the world; production wiring is [`crate::gc::gc_sweep`] /
/// [`crate::gc::gc_sweep_dry_run`]. `agents_read` is the same kind of seam
/// for the `claude agents --json --all` snapshot: read at most once per
/// sweep, lazily, only when a row actually reaches the stop gate - steady
/// state keeps zero subprocesses on the hot path.
///
/// `age_many` is the transcript-age seam (x-54cf): one batched call answers
/// every candidate row's age in SECONDS, keyed by [`row_handle`]. The
/// production default reads the newest timestamped transcript entry through
/// the shared truth probe; a file stat was the retired instrument, because
/// untimestamped trailing records keep a dead file reading fresh. A row the
/// seam does not answer reads `None`, and `None` is never quiet.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run(
    home: &AgentsHome,
    emitter: &EventEmitter,
    grace_secs: i64,
    dry_run: bool,
    retain_days: u64,
    read_graph: &dyn Fn(&AgentsHome) -> Option<GraphRead>,
    store_matches: &dyn Fn(&state::RegistryEntry) -> Option<Vec<PathBuf>>,
    age_many: &dyn Fn(&[&state::RegistryEntry]) -> HashMap<String, Option<i64>>,
    stop_confirmed: &dyn Fn(&state::RegistryEntry) -> bool,
    surface_removal: &dyn Fn(&state::RegistryEntry) -> crate::daemon::CascadeOutcome,
    agents_read: &dyn Fn() -> crate::claude_roster::ClaudeAgentsSnapshot,
    tree_probe: &dyn Fn(&state::RegistryEntry) -> (Option<bool>, Option<bool>),
    prune_tree: &dyn Fn(&state::RegistryEntry) -> Option<crate::daemon::PruneOutcome>,
) -> GcSummary {
    run_with_release(
        home,
        emitter,
        grace_secs,
        dry_run,
        retain_days,
        read_graph,
        store_matches,
        age_many,
        stop_confirmed,
        surface_removal,
        agents_read,
        tree_probe,
        prune_tree,
        None,
    )
}

/// [`run`] with a release ruling (x-e3cc): the sweep lifts exactly one gate
/// on the matching row, and only while its current hold still equals the
/// one the release captured. Every other gate stays.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_with_release(
    home: &AgentsHome,
    emitter: &EventEmitter,
    grace_secs: i64,
    dry_run: bool,
    retain_days: u64,
    read_graph: &dyn Fn(&AgentsHome) -> Option<GraphRead>,
    store_matches: &dyn Fn(&state::RegistryEntry) -> Option<Vec<PathBuf>>,
    age_many: &dyn Fn(&[&state::RegistryEntry]) -> HashMap<String, Option<i64>>,
    stop_confirmed: &dyn Fn(&state::RegistryEntry) -> bool,
    surface_removal: &dyn Fn(&state::RegistryEntry) -> crate::daemon::CascadeOutcome,
    agents_read: &dyn Fn() -> crate::claude_roster::ClaudeAgentsSnapshot,
    tree_probe: &dyn Fn(&state::RegistryEntry) -> (Option<bool>, Option<bool>),
    prune_tree: &dyn Fn(&state::RegistryEntry) -> Option<crate::daemon::PruneOutcome>,
    release: Option<&Release>,
) -> GcSummary {
    let mut summary = GcSummary::default();
    // The retention pass runs on EVERY sweep, before the empty-registry early
    // return: receipts age out on their own clock. Any receipt this pass goes
    // on to write carries `reaped_at` of now, so it can never be this
    // expiry's victim.
    if !dry_run {
        expire_reap_receipts(home, retain_days, &mut summary);
    }
    let (registry, registry_read) = match state::load_registry(&home.registry_json()) {
        Ok(r) => (r, true),
        Err(_) => (Default::default(), false),
    };
    if !registry_read {
        // A read that failed is not a census of zero: the tick must render a
        // failed sweep, not a quiet no_rows.
        summary.registry_unreadable = true;
    }
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
    // that reaches the stop gate - never on the empty/kept hot path. The
    // terminal-state read (x-2774 change 1) shares the memo: an Open claude
    // row triggers the same one-snapshot read, so a fleet still pays at
    // most one `claude agents` per sweep.
    let agents_memo: std::cell::RefCell<Option<crate::claude_roster::ClaudeAgentsSnapshot>> =
        std::cell::RefCell::new(None);

    // Pass 1 (x-2774 change 3): prove provenance and transcript age ONCE per
    // spawn row, so the supersession map and the row pass read the same
    // verdict instead of answering the reverse join twice. Entries the
    // origin gates already hold stay None - their buckets are decided in
    // pass 2 without a verdict.
    // One batched age read for the whole sweep (x-54cf): the seam answers
    // every candidate through one single-flighted read, keyed by row handle.
    // A row the seam does not answer reads None, and None is never quiet.
    let age_entries: Vec<&state::RegistryEntry> = registry
        .entries
        .iter()
        .filter(|e| {
            e.origin.as_deref() == Some("spawn") && e.crown_level.is_none() && graph.is_some()
        })
        .collect();
    let ages = age_many(&age_entries);
    let mut staged: Vec<Option<(ProvenanceVerdict, Option<i64>)>> =
        Vec::with_capacity(registry.entries.len());
    for e in &registry.entries {
        let eligible = e.origin.as_deref() == Some("spawn") && e.crown_level.is_none();
        if !eligible {
            staged.push(None);
            continue;
        }
        let Some(graph) = &graph else {
            staged.push(None);
            continue;
        };
        let sid = e.harness_session_id.as_deref().unwrap_or("").trim();
        let hits = store_matches(e);
        let verdict = provenance_verdict(e, sid, graph, hits.as_deref());
        let age = ages.get(&row_handle(e)).copied().flatten();
        staged.push(Some((verdict, age)));
    }

    // The live-peer map (x-2774 change 3): node -> (name, created_at) of the
    // NEWEST spawn row on that node whose transcript is inside the grace
    // window. Both halves are positive markers - a newer spawn exists and it
    // is demonstrably live - so a lone worker is never superseded and two
    // quiet peers never sweep each other. Ties resolve by created_at then
    // name so the map never depends on registry order.
    let mut live_peer: HashMap<String, (String, String)> = HashMap::new();
    for (e, staged_row) in registry.entries.iter().zip(staged.iter()) {
        let Some((verdict, age)) = staged_row else {
            continue;
        };
        let fresh = matches!(age, Some(a) if *a <= grace_secs);
        if !fresh {
            continue;
        }
        if let WorkState::Open { node, .. } = &verdict.work {
            let take = match live_peer.get(node) {
                None => true,
                Some((_, created)) if created.as_str() < e.created_at.as_str() => true,
                Some((name, created)) => {
                    created.as_str() == e.created_at.as_str() && name.as_str() < e.name.as_str()
                }
                Some((_, _)) => false,
            };
            if take {
                live_peer.insert(node.clone(), (e.name.clone(), e.created_at.clone()));
            }
        }
    }

    for (e, staged_row) in registry.entries.iter().zip(staged.iter()) {
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
        let Some((verdict, age)) = staged_row else {
            // A row pass 1 did not stage: the graph read failed there too.
            // Same bucket as a failed direct read - never a retirement on a
            // failed read.
            summary.kept_graph_unreadable.push(id);
            continue;
        };
        let sid = e.harness_session_id.as_deref().unwrap_or("").trim();
        let mut work = verdict.work.clone();
        // (x-1b90 change 3) Every node the row names reads done - the exact
        // condition under which an old unresolved hold may ask for a
        // decision. Computed before `work` moves into the GcRow.
        let nodes_done = matches!(work, WorkState::AllDone { .. });
        // The hold clock (x-e3cc): computed once per row, read by every hold
        // push below. A keep without an age is a reader that never ran.
        let (hold_age_s, hold_age_basis) = hold_clock(*age, &e.created_at, now);
        // The release's per-row state (x-e3cc): the lifts and the note the
        // retire basis carries. `None` note after all gates ran means a
        // release named this row but matched none of its holds.
        let release_for_row = release.filter(|r| r.handle == id);
        let mut release_note: Option<String> = None;
        let mut release_quiet_row = false;
        let mut release_stop = false;
        // Locked Decision 1: every named node done but one still carries an
        // OPEN do row for this session -> the row stays and the node is
        // named. The retirement never settles graph rows itself.
        if matches!(work, WorkState::AllDone { .. }) {
            if let Some(nodes) = graph.open_do.get(&sid.to_ascii_lowercase()) {
                let node = nodes.first().cloned().unwrap_or_default();
                let reason = KeepReason::OpenDoRow { node: node.clone() }.as_str();
                let detail = settle_blocker_detail(graph, &node);
                // The release settles this one obligation through the
                // settle's own door (x-e3cc): fill-if-absent, ended_by
                // reap-release, the same blocker gate the batch settle
                // applies. A write that refuses keeps the row.
                if release_for_row.is_some_and(|r| r.matches(reason, &detail)) {
                    match settle_one_do_row(home, &node, sid) {
                        Ok(true) => {
                            release_note = Some(release_basis_prefix(reason, hold_age_s, &detail));
                        }
                        Ok(false) => {
                            summary.release_refused.push(format!(
                                "{id}: release refused: the open do row on {node} is not fillable: {detail}"
                            ));
                            summary.kept_open_do_row.push((id.clone(), node.clone()));
                            summary.holds.push(Hold {
                                id,
                                reason,
                                detail,
                                age_s: hold_age_s,
                                age_basis: hold_age_basis,
                                escalated: false,
                            });
                            continue;
                        }
                        Err(refused) => {
                            summary.settle_refused.push((id.clone(), refused));
                            summary.kept_open_do_row.push((id.clone(), node.clone()));
                            summary.holds.push(Hold {
                                id,
                                reason,
                                detail,
                                age_s: hold_age_s,
                                age_basis: hold_age_basis,
                                escalated: false,
                            });
                            continue;
                        }
                    }
                } else {
                    if let Some(r) = release_for_row {
                        summary.release_refused.push(format!(
                            "{id}: release refused: hold changed from {} ({}) to {reason} ({detail})",
                            r.reason, r.detail
                        ));
                    }
                    summary.kept_open_do_row.push((id.clone(), node.clone()));
                    summary.holds.push(Hold {
                        id,
                        reason,
                        detail,
                        age_s: hold_age_s,
                        age_basis: hold_age_basis,
                        escalated: false,
                    });
                    continue;
                }
            }
        }
        let mut confirm_hold = verdict.hold.clone();
        // x-e3cc: the release reads the conflict hold as agreement - the
        // witness set (first source's node plus the dissenting node) is all
        // done, so work reads AllDone over that set and the hold lifts. The
        // not-done-witness refusal is the classify phase's job; this is the
        // apply of a ruling already checked.
        if let Some(r) = release_for_row {
            if let Some(KeepReason::NodeConflict { a, b }) = &confirm_hold {
                let detail = format!("{a} vs {b}");
                let reason = KeepReason::NodeConflict {
                    a: a.clone(),
                    b: b.clone(),
                }
                .as_str();
                if r.matches(reason, &detail) {
                    // Both sides of the hold are "<source> <node>" strings
                    // (x-e3cc change 1), so the dissenting NODE is the last
                    // token, never the whole side.
                    let dissent_node = b.split_whitespace().last().unwrap_or(b).to_string();
                    let mut nodes = vec![verdict.route.node.clone().unwrap_or_default()];
                    nodes.push(dissent_node);
                    nodes.dedup();
                    work = WorkState::AllDone { nodes };
                    confirm_hold = None;
                    release_note = Some(release_basis_prefix(reason, hold_age_s, &detail));
                }
            }
        }
        let merge_note = verdict.merge_note.clone();
        let age = *age;
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
        // x-2774 changes 1, 3 and 6: the session-shaped releases. The
        // harness's terminal state, a live newer peer on the same node, a
        // parked or never-started node, and a recorded merge the status lags
        // all answer the same question - is THIS session's own story over -
        // and each falls through to the grace gate in gc_decide.
        // x-b7f8: EVERY claude row carries its terminal state, not only
        // Open-work rows - recency and lineage must be able to yield to it.
        let session_terminal = if e.harness_name() == "claude" {
            let mut memo = agents_memo.borrow_mut();
            let snapshot = memo.get_or_insert_with(&agents_read);
            crate::daemon::claude_row_id(e)
                .and_then(|rid| snapshot.find(&rid).cloned())
                .and_then(|row| row.state)
                .filter(|s| crate::claude_roster::is_terminal_roster_state(s))
        } else {
            None
        };
        let superseded_by_live_peer = match &work {
            WorkState::Open { node, .. } => live_peer
                .get(node)
                .filter(|(peer_name, peer_created)| {
                    peer_name != &e.name && peer_created.as_str() > e.created_at.as_str()
                })
                .map(|(name, created)| format!("{name} (created {created})")),
            _ => None,
        };
        let node_merged = verdict.merged_but_open.is_some();
        // x-2774 change 8: the existence-specific probe on the row's own
        // pid. One kill(2) per row, no subprocess; only ESRCH counts.
        let pid_gone = e.pid.is_some_and(crate::daemon::pid_is_gone);
        // x-e3cc: the remaining two lifts. A release for a
        // transcript-unresolved hold reads the missing age as quiet for this
        // row only (grace_gate's release_quiet arm). A stop-family release
        // satisfies the stop gate whether or not absence confirms; the stop
        // is still issued below.
        if let Some(r) = release_for_row {
            if release_note.is_none()
                && confirm_hold.is_none()
                && r.matches(
                    KeepReason::TranscriptUnresolved.as_str(),
                    "absence is not quiet",
                )
            {
                release_quiet_row = true;
                release_note = Some(release_basis_prefix(
                    KeepReason::TranscriptUnresolved.as_str(),
                    hold_age_s,
                    "absence is not quiet",
                ));
            } else if release_note.is_none() && confirm_hold.is_none() && r.is_stop_family() {
                release_stop = true;
                release_note = Some(release_basis_prefix(&r.reason, hold_age_s, &r.detail));
            }
        }
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
            session_terminal,
            superseded_by_live_peer,
            node_merged,
            pid_gone,
            release_quiet: release_quiet_row,
        };
        let (action, reason) = gc_decide(&row, grace_secs);
        if action == GcAction::Keep {
            if let Some(r) = release_for_row {
                if release_note.is_none() {
                    summary.release_refused.push(format!(
                        "{id}: release refused: hold changed from {} ({}) to {:?}",
                        r.reason, r.detail, reason
                    ));
                }
            }
            match reason {
                Some(KeepReason::Operator) => summary.kept_operator.push(id),
                Some(KeepReason::Crowned) => summary.kept_crowned.push(id),
                Some(KeepReason::NotSpawn { origin }) => summary.kept_not_spawn.push((id, origin)),
                Some(KeepReason::NoProvenance) => summary.kept_no_provenance.push(id),
                Some(KeepReason::OpenWork { node, status }) => {
                    let reader = verdict
                        .route
                        .source
                        .map(|s| s.as_str())
                        .unwrap_or("sessions")
                        .to_string();
                    summary.kept_open_work.push((id, node, status, reader))
                }
                Some(KeepReason::Active { age_s }) => summary.kept_active.push((id, age_s)),
                Some(KeepReason::TranscriptUnresolved) => {
                    // Main's x-1b90 bucket carries the clock the TU line
                    // renders; the holds projection stays the one answer for
                    // every hold kind the question lane and the release verb
                    // read.
                    summary.kept_transcript_unresolved.push(UnresolvedHold {
                        id: id.clone(),
                        held_s: unresolved_hold_secs(e, now),
                        nodes_done,
                    });
                    summary.holds.push(Hold {
                        id,
                        reason: KeepReason::TranscriptUnresolved.as_str(),
                        detail: "absence is not quiet".into(),
                        age_s: hold_age_s,
                        age_basis: hold_age_basis,
                        escalated: false,
                    });
                }
                Some(KeepReason::NodeConflict { a, b }) => {
                    summary
                        .kept_node_conflict
                        .push((id.clone(), a.clone(), b.clone()));
                    summary.holds.push(Hold {
                        id,
                        reason: KeepReason::NodeConflict {
                            a: a.clone(),
                            b: b.clone(),
                        }
                        .as_str(),
                        detail: format!("{a} vs {b}"),
                        age_s: hold_age_s,
                        age_basis: hold_age_basis,
                        escalated: false,
                    });
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
        // x-b7f8: one conjunct - a parent whose own harness reports a
        // terminal state is not held. The guard's harm (a parent's native
        // surface archived while children still run) needs a RUNNING
        // parent; the shared-worktree guard below still protects a live
        // child's tree.
        if !sid.is_empty() && row.session_terminal.is_none() {
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
            // The re-read rides the same age seam (x-54cf): a fresh answer for
            // THIS row, so activity inside the classify-to-apply gap keeps.
            let fresh_age = age_many(&[e]).get(&row_handle(e)).copied().flatten();
            // x-2774 change 8: activity without a living writer is not
            // activity. A pid that answered ESRCH at the re-check keeps its
            // retirement even if the transcript mtime moved - the write came
            // from something else. The age and pid witnesses live in
            // `worker_finished`; the roster witness stays here as the
            // x-b7f8 gap clause, because only this sweep compares the fresh
            // read against its classification age.
            // x-b7f8: for a terminal row the re-check asks one question -
            // did the session write since classification. A smaller fresh
            // age IS a new write (the session came back, perhaps through a
            // mail inject); an equal-or-older one is not. An unresolved
            // re-read keeps: absence is not quiet.
            // ponytail: a write inside the same whole second as a
            // classification that already read age 0 is not seen.
            let still_quiet = worker_finished(e, fresh_age, grace_secs, None)
                || (release_quiet_row && fresh_age.is_none())
                || (row.session_terminal.is_some()
                    && matches!((fresh_age, age), (Some(now_a), Some(then_a)) if now_a >= then_a));
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
        if dry_run && death.is_none() && e.harness_name() == "claude" && !release_stop {
            let detail = "no terminal roster state and no dead pid; a dry run does not \
                 promise a stop it cannot prove"
                .to_string();
            if let Some(r) = release_for_row {
                if release_note.is_none() {
                    summary.release_refused.push(format!(
                        "{id}: release refused: hold changed from {} ({}) to needs live stop ({detail})",
                        r.reason, r.detail
                    ));
                }
            }
            summary.needs_live_stop.push((id.clone(), detail.clone()));
            summary.holds.push(Hold {
                id,
                reason: "needs live stop",
                detail,
                age_s: hold_age_s,
                age_basis: hold_age_basis,
                escalated: false,
            });
            continue;
        }
        // x-58a5: the same honesty for pane rows. The precheck answers what
        // the real run's stop will answer - already-stopped retires, a live
        // pid is a kill no dry run may promise, an unprovable pid is a
        // refusal both runs share. No release lift here: a release
        // satisfies only the stop_on_death seam, and the pane stop never
        // consults that seam, so the real pane stop refuses under one and
        // the dry run must predict that.
        if dry_run && e.substrate.as_deref() == Some("pane") {
            match crate::pane_stop::precheck_pane_stop(e) {
                crate::pane_stop::PanePrecheck::AlreadyStopped(_) => {}
                crate::pane_stop::PanePrecheck::NeedsKill => {
                    let detail = "pane pid is running; a dry run does not promise a kill \
                                  it cannot prove"
                        .to_string();
                    if let Some(r) = release_for_row {
                        if release_note.is_none() {
                            summary.release_refused.push(format!(
                                "{id}: release refused: hold changed from {} ({}) to needs live stop ({detail})",
                                r.reason, r.detail
                            ));
                        }
                    }
                    summary.needs_live_stop.push((id.clone(), detail.clone()));
                    summary.holds.push(Hold {
                        id,
                        reason: "needs live stop",
                        detail,
                        age_s: hold_age_s,
                        age_basis: hold_age_basis,
                        escalated: false,
                    });
                    continue;
                }
                crate::pane_stop::PanePrecheck::Unprovable(detail) => {
                    if let Some(r) = release_for_row {
                        if release_note.is_none() {
                            summary.release_refused.push(format!(
                                "{id}: release refused: hold changed from {} ({}) to stop refused ({detail})",
                                r.reason, r.detail
                            ));
                        }
                    }
                    summary.stop_refused.push((id.clone(), detail.clone()));
                    summary.holds.push(Hold {
                        id,
                        reason: "stop refused",
                        detail,
                        age_s: hold_age_s,
                        age_basis: hold_age_basis,
                        escalated: false,
                    });
                    continue;
                }
            }
        }
        // The stop-family release still ISSUES the stop (x-e3cc): the seam
        // runs, the receipt records the outcome, and the release satisfies
        // the gate whether or not absence confirms. `None` when death
        // evidence already answered or no release rides.
        let release_stop_outcome: Option<bool> = if release_stop && death.is_none() {
            Some(stop_confirmed(e))
        } else {
            None
        };
        // Death evidence satisfies the stop. It wraps the callee's seam here,
        // in the caller that owns the snapshot, so the shared signature is
        // untouched.
        let stop_on_death =
            |entry: &state::RegistryEntry| death.is_some() || release_stop || stop_confirmed(entry);
        // x-2774: the session-shaped release that let an OPEN-work row
        // retire. The obligation re-checks (stage and commit) yield to it.
        let released = row.session_released();
        if let Err(refusal) = stage_session_retirement(
            home,
            e,
            ledger.as_deref(),
            dry_run,
            released,
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
                    summary.stop_refused.push((id.clone(), reason.clone()));
                    summary.holds.push(Hold {
                        id,
                        reason: "stop refused",
                        detail: reason,
                        age_s: hold_age_s,
                        age_basis: hold_age_basis,
                        escalated: false,
                    });
                }
                RetireRefusal::NativeRemoval(reason) => {
                    summary.stop_refused.push((id.clone(), reason.clone()));
                    summary.holds.push(Hold {
                        id,
                        reason: "stop refused",
                        detail: reason,
                        age_s: hold_age_s,
                        age_basis: hold_age_basis,
                        escalated: false,
                    });
                }
                RetireRefusal::NoReceipt(reason) => summary.kept_no_receipt.push((id, reason)),
                RetireRefusal::GraphObligation(node) => {
                    summary.kept_open_do_row.push((id.clone(), node.clone()));
                    summary.holds.push(Hold {
                        id,
                        reason: KeepReason::OpenDoRow { node: node.clone() }.as_str(),
                        detail: settle_blocker_detail(graph, &node),
                        age_s: hold_age_s,
                        age_basis: hold_age_basis,
                        escalated: false,
                    });
                }
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
            WorkState::Open { node, status } => {
                // x-2774 change 2: an Open-work retirement is now reachable,
                // and it is never anonymous. The arm order mirrors the
                // release precedence in gc_decide: terminal state, then live
                // peer, then inactive status, then recorded merge.
                if let Some(peer) = &probed.superseded_by_live_peer {
                    format!("superseded on {node} by live peer {peer}")
                } else if let Some(state) = &probed.session_terminal {
                    format!(
                        "session terminal: harness state {state} (via {}); node {node} {status}",
                        via.as_str()
                    )
                } else if crate::gc::INACTIVE_NODE_STATUSES.contains(&status.as_str()) {
                    format!("node {node} is {status}, not active work")
                } else {
                    format!("node {node} {status}; recorded merge_status merged")
                }
            }
            _ => "done".to_string(), // unreachable: only AllDone and the released Open arms retire
        };
        // x-2774 change 8: name the early fire in the audit line. A basis
        // that reads "quiet past grace" when the transcript was actually
        // inside grace misreports why the row went; the pid evidence is the
        // reason it went when it did.
        let mut basis =
            if probed.pid_gone && probed.transcript_age_s.is_some_and(|age| age <= grace_secs) {
                format!("{basis}; pid {} is gone", e.pid.unwrap_or(0))
            } else {
                basis
            };
        // x-b7f8: name the early fire for a terminal state the way the pid
        // evidence names its own. An AllDone row retiring INSIDE the grace
        // window went because the harness says the session finished, not
        // because the transcript aged out; the Open basis already names the
        // state in its own arm.
        if matches!(probed.work, WorkState::AllDone { .. })
            && probed.session_terminal.is_some()
            && probed.transcript_age_s.is_some_and(|age| age <= grace_secs)
        {
            basis = format!(
                "{basis}; session terminal: harness state {}",
                probed.session_terminal.clone().unwrap_or_default()
            );
        }
        // The release rides the audit line (x-e3cc): what was ruled, how old
        // the hold was, and an unconfirmed stop named as such.
        if let Some(note) = &release_note {
            basis = format!("{note}{basis}");
        }
        if release_stop_outcome == Some(false) {
            basis.push_str("; stop issued, unconfirmed");
        }
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
                released,
                via_release: release_note.is_some(),
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
    session_released: bool,
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
    // x-2774: the re-check yields to a session-shaped release - a session
    // released by terminal harness state, a live newer peer, a parked node,
    // or a recorded merge has finished its own story on this node. Its open
    // do row is the stale record of exactly the work that moved on (to the
    // peer, to the merge, to the park), and the settle or reconcile owns
    // closing it. Holding the row here would re-hold every released row one
    // gate later, on the very obligation the release just resolved. The
    // grace and freshness gates still protect a genuinely live session.
    if !dry_run && !session_released {
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
    // Effect 1: the confirmed stop of the held process. Pane-substrate rows
    // stop through the pid-proving helper (x-1b90 change 1): the roster and
    // the worker socket never held the pane, so both today's arms confirm a
    // stop that never happened. The detail is the receipt's measurement, so
    // the routing decision lives here where the effect is built - never in
    // the seam closure, whose bool answer cannot carry it.
    let pane_stop = if e.substrate.as_deref() == Some("pane") {
        Some(crate::pane_stop::stop_pane_process_confirmed(e))
    } else {
        None
    };
    let stopped = pane_stop
        .as_ref()
        .map(|s| s.confirmed)
        .unwrap_or_else(|| stop_confirmed(e));
    // x-9485 change 1: a confirmed stop names what it stopped. The pane arm
    // carries what actually ran (x-1b90); the other arms go through the
    // injected bool seam, so the detail names the arm and the row's
    // registered pid - the process the outcome is about - and the checked-in
    // probe verifies that pid reads gone.
    let stop_detail = pane_stop.as_ref().map(|s| s.detail.clone()).or_else(|| {
        if stopped {
            stop_row_detail(e)
        } else {
            None
        }
    });
    receipt
        .effects
        .push(crate::gc_native::stop_outcome_effect(stopped, stop_detail));
    if !stopped {
        let _ = write_reap_receipt(home, &receipt);
        return Err(RetireRefusal::StopRefused(
            pane_stop
                .map(|s| s.detail)
                .unwrap_or_else(|| "the stop did not confirm; row kept for retry".into()),
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
                // x-2774: a session-shaped release outranks the row's own
                // stale do row here too, the same yield the stage-time
                // re-check makes.
                .filter(|(_, order)| !order.released)
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
                let _ = emitter.emit("agent_row_reaped", &{
                    let mut event = json!({
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
                    });
                    // The release names itself as the remover (x-e3cc): a
                    // retirement an operator ruling applied is auditable
                    // as one.
                    if order.via_release {
                        event["remover"] = json!("reap --release");
                    }
                    event
                });
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
        // A writer creates the sidecar before it touches the row, so no sidecar
        // means no writer to serialize against. Requiring one here would strand
        // every row whose lock aged out first: locks_retain_days is shorter than
        // pr_status_cache_retain_days, and the lock family sweeps after this one.
        if !lock_path.exists() {
            match opened_path_matches(&row, &path) {
                Ok(true) => {
                    record_state_file_action(&path, name, metadata.len(), age_s, apply, summary)
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
            .create(false)
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
    claims_dir: PathBuf,
    locks_root: PathBuf,
    agents_root: PathBuf,
    agents_dir: PathBuf,
    state_root: PathBuf,
    pr_status_dir: PathBuf,
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
        &roots.claims_dir.join(".expired"),
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
        &roots.agents_dir.join("locks"),
        config.locks_retain_days,
        apply,
        &mut summary.agent_locks,
        &|_| {},
    );
    reap_pr_status_rows(
        &roots.state_root,
        &roots.pr_status_dir,
        config.pr_status_cache_retain_days,
        apply,
        &mut summary.pr_status_cache,
    );
    reap_lock_family(
        &roots.state_root,
        &roots.pr_status_dir,
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
            claims_dir: root.join("claims"),
            locks_root: root.to_path_buf(),
            agents_root: root.to_path_buf(),
            agents_dir: root.join("agents"),
            state_root: root.to_path_buf(),
            pr_status_dir: root.join("cache/pr-status"),
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
    let Some(claims_dir) = crate::claims::claims_dir_for(None) else {
        return unavailable_state_reap("claims root unavailable");
    };
    let Some(locks_dir) = crate::agents_config::machine_locks_dir() else {
        return unavailable_state_reap("machine locks root unavailable");
    };
    let Some(state_root) = crate::agents_config::state_dir(cwd) else {
        return unavailable_state_reap("state root unavailable");
    };
    let Some(pr_status_dir) = crate::agents_config::pr_status_cache_dir(cwd) else {
        return unavailable_state_reap("PR-status cache root unavailable");
    };
    let Some(locks_root) = locks_dir.parent().map(Path::to_path_buf) else {
        return unavailable_state_reap("machine locks root unavailable");
    };
    reap_state_files_with_roots(
        StateReapRoots {
            claims_root,
            claims_dir,
            locks_root,
            agents_root: home.root().parent().unwrap_or(home.root()).to_path_buf(),
            agents_dir: home.root().to_path_buf(),
            state_root,
            pr_status_dir,
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

pub fn state_reap_has_failures(summary: &StateFilesReapSummary) -> bool {
    summary.skip_reason.is_some()
        || [
            &summary.expired_claims,
            &summary.plan_locks,
            &summary.agent_locks,
            &summary.pr_status_cache,
        ]
        .iter()
        .flat_map(|family| family.kept.iter())
        .any(|kept| {
            kept.reason.contains("failed")
                || kept.reason.contains("unavailable")
                || kept.reason.contains("revalidation")
        })
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

    #[test]
    fn settle_backoff_is_full_jitter_within_the_attempt_bound() {
        // The x-1601 shape: uniform in [0, min(cap, base << attempt)], so two
        // sweepers never re-line up on the same instant the way the flat 250
        // ms sleep did.
        for attempt in 0..7 {
            let bound = SETTLE_BACKOFF_CAP_MS.min(SETTLE_BACKOFF_BASE_MS << attempt);
            for _ in 0..64 {
                let delay = settle_backoff_ms(attempt);
                assert!(delay <= bound, "attempt {attempt}: {delay} > {bound}");
            }
        }
        // The cap holds at the ceiling no matter how far the shift climbs.
        assert_eq!(
            SETTLE_BACKOFF_CAP_MS.min(SETTLE_BACKOFF_BASE_MS << 20),
            SETTLE_BACKOFF_CAP_MS
        );
    }

    #[test]
    fn worker_finished_reads_three_witnesses() {
        let grace = 900i64;
        let mut e = state::RegistryEntry::default();
        // Witness 1: a transcript older than the grace window.
        assert!(worker_finished(&e, Some(901), grace, None));
        // Witness 2: a held pid that answers ESRCH.
        e.pid = Some(2_000_000_000);
        assert!(worker_finished(&e, Some(10), grace, None));
        // Witness 3: a terminal state on the roster outranks a fresh
        // transcript.
        assert!(worker_finished(
            &e,
            Some(5),
            grace,
            Some("row p present, state done")
        ));
        // Nothing decisive: not finished, liveness alone never vetoes a
        // retirement but never proves one either.
        e.pid = None;
        assert!(!worker_finished(&e, Some(20), grace, None));
        // An unresolved transcript is not a quiet one.
        assert!(!worker_finished(&e, None, grace, None));
    }

    // x-9485 change 1: the non-pane stop arms name the arm and the process
    // the receipt's confirmed-removed outcome is about, so the checked-in
    // probe can verify the pid reads gone.
    #[test]
    fn stop_row_detail_names_the_arm_and_the_registered_pid() {
        let mut claude = state::RegistryEntry::default();
        claude.harness = Some("claude".into());
        claude.harness_session_id = Some("aaaa-bbbb".into());
        claude.pid = Some(40001);
        assert_eq!(
            stop_row_detail(&claude).as_deref(),
            Some("claude stop ran; session aaaa-bbbb")
        );

        let mut codex = state::RegistryEntry::default();
        codex.harness = Some("codex".into());
        codex.harness_session_id = Some("01a08db0-0000".into());
        codex.pid = Some(50678);
        assert_eq!(
            stop_row_detail(&codex).as_deref(),
            Some("worker socket stop ran; pid 50678")
        );

        // No pid: the row is still named, the pid slot is not invented.
        codex.pid = None;
        assert_eq!(
            stop_row_detail(&codex).as_deref(),
            Some("worker socket stop ran")
        );

        // A claude row with no session id has nothing honest to say.
        claude.harness_session_id = None;
        assert_eq!(stop_row_detail(&claude), None);
    }

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
            &|_| std::collections::HashMap::new(),
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

    /// The default windows retire a sidecar before its row is even eligible
    /// (7 days against 14, lock family second). A row that outlives its lock
    /// must still be reapable, or the family stops cleaning after one week.
    #[test]
    fn state_reap_still_removes_a_row_whose_lock_aged_out_first() {
        let (base, home) = stale_state_home("row-outlives-lock");
        let statuses = base.join("cache/pr-status");
        std::fs::create_dir_all(&statuses).unwrap();
        let row = statuses.join("42.json");
        let lock = statuses.join("42.lock");
        std::fs::write(&row, b"{}").unwrap();
        std::fs::write(&lock, b"").unwrap();
        age_file(&row, 10);
        age_file(&lock, 10);

        let first = reap_state_files(
            &home,
            crate::agents_config::StateReapConfig::default(),
            true,
        );

        assert!(!lock.exists(), "the lock is past its 7-day window");
        assert!(row.exists(), "the row is still inside its 14-day window");
        assert_eq!(first.pr_status_cache.deleted, 1);

        age_file(&row, 20);
        let second = reap_state_files(
            &home,
            crate::agents_config::StateReapConfig::default(),
            true,
        );

        assert!(!row.exists(), "an orphaned row must not outlive its window");
        assert_eq!(second.pr_status_cache.deleted, 1);
        assert!(second.pr_status_cache.kept.is_empty());
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
            pr_state: HashMap::from([("N1".to_string(), (Some("merged".into()), 0, 0))]),
            ..Default::default()
        };
        let verdict = provenance_verdict(&e, "sid-77", &graph, None);
        assert_eq!(
            verdict.hold,
            Some(KeepReason::NodeConflict {
                // Both witnesses ride the hold (x-e3cc): the first source's
                // answer names itself beside the dissenting one.
                a: "sessions N1".into(),
                b: "name N2".into()
            }),
            "the contradicting witness holds the row"
        );
        assert!(
            matches!(verdict.work, WorkState::NoProvenance),
            "a conflict leaves no work verdict to retire on"
        );
    }
}
