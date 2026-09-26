//! The merge reaper : a PR merged, its node done, the machine reaps.
//!
//! `fno do pr merge` mints one `merge_cleanup_requested` envelope the moment
//! the merge confirms (the post-merge ritual is the second mint site and
//! folds into the same request id). This module consumes those requests on
//! the daemon tick: a grace window anchored at `merged_at`, a doneness
//! re-read against the graph, the harness stop FIRST, then the registry row,
//! then the tree. That order is load-bearing: dropping the row before the
//! harness stop orphans the session in its sideline, and the recovery is
//! `claude adopt` plus a second removal.
//!
//! Only the trigger, the doneness re-read and the TREE are this module's own.
//! The session half of each retirement - stop, the native active-surface
//! removal, the resumable receipt, the guarded registry write - is
//! `gc_sweep`'s, and this module calls it. One sequence, so a merge-triggered
//! removal and a scheduled one leave the same record.
//!
//! This is the ONLY bound the machine has on registry row count: spawn_gate
//! counts a row only while its pid is alive (or its short id is in the live
//! roster), so the dead population is invisible to every cap. Grace is a
//! clock, never a liveness probe: an idle KING reads state=done, so candidacy
//! never keys on roster state, and crowned and operator-origin rows are
//! excluded by name in the receipt.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::{json, Value};

use crate::events::EventEmitter;
use crate::paths::AgentsHome;
use crate::state;

/// How long between reaper passes. Today the consumer rescanned the whole
/// events journal on every 5s daemon tick; a merge is minutes old at best,
/// so a 60s floor loses nothing and the pass stays off the tick's hot path.
const MERGE_REAP_INTERVAL_SECS: u64 = 60;

/// A request older than this past its mint moment expires instead of pinning
/// forever. Grace still counts from the merge moment; this window bounds late
/// ritual mints without expiring a request before its first reaper pass.
const MERGE_REAP_EXPIRY_SECS: i64 = 86_400;

/// One held request names its reason at most once an hour, not once per 60s
/// pass: the hold is the normal shape while a worker's last writes settle,
/// and a per-pass event would outshout the events that matter.
const MERGE_REAP_HOLD_ECHO_SECS: i64 = 3_600;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MergeCleanupRequest {
    request_id: String,
    repo: String,
    pr: i64,
    branch: Option<String>,
    worktree: Option<String>,
    node_ids: Vec<String>,
    /// The exact registered row names the merge producer proposed.
    /// Non-empty: rows are selected by exact membership (plus worktree
    /// equality) and the reaper never re-derives names. Empty: an event older
    /// than the field, and the ONLY case where the narrow legacy
    /// `target-<node>-` prefix fallback runs.
    candidate_row_names: Vec<String>,
    /// Unix seconds: when the merge landed (the grace anchor). A request
    /// without one (an older ritual mint) falls back to the envelope `ts`.
    merged_at: Option<i64>,
    ts_unix: i64,
    session_id: Option<String>,
    harness: Option<String>,
}

/// Every pending request across repos, in one store-aware journal read:
/// duplicate envelopes that share a request id (the merge mint and the
/// ritual mint for one merge) fold field by field.
fn pending_merge_cleanup_requests_all(home: &AgentsHome) -> Vec<MergeCleanupRequest> {
    let (requested, finished) = scan_merge_cleanup_events(home);
    requested
        .into_values()
        .filter(|request| !finished.contains(&request.request_id))
        .collect()
}

/// Every repo a merge-cleanup request names, pending or settled: what
/// `registry_repo_roots` collects, so a repo whose request left the live
/// file stays in the reaper's roots.
pub(crate) fn merge_cleanup_request_repos(home: &AgentsHome) -> Vec<String> {
    let (requested, _) = scan_merge_cleanup_events(home);
    let repos: HashSet<String> = requested.into_values().map(|r| r.repo).collect();
    repos.into_iter().collect()
}

/// The merge-cleanup kinds the reaper folds, one vocabulary for the reader.
const MERGE_CLEANUP_TYPES: &[&str] = &[
    "merge_cleanup_requested",
    "merge_cleanup_completed",
    "merge_cleanup_refused",
    "merge_cleanup_expired",
];

/// The merge-cleanup envelopes across one journal's committed rows plus the
/// unseen live lines, folded per request id, plus the settled tombstone ids.
/// A line that does not name a merge_cleanup_ kind is skipped before JSON
/// parsing.
fn scan_merge_cleanup_events(
    home: &AgentsHome,
) -> (BTreeMap<String, MergeCleanupRequest>, HashSet<String>) {
    let contents = crate::event_store::journal_text(&home.events_jsonl(), MERGE_CLEANUP_TYPES);
    let mut requested = BTreeMap::<String, MergeCleanupRequest>::new();
    let mut finished = HashSet::<String>::new();
    for line in contents.lines() {
        if !line.contains("\"merge_cleanup_") {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(kind) = event.get("type").and_then(Value::as_str) else {
            continue;
        };
        let Some(data) = event.get("data") else {
            continue;
        };
        let Some(request_id) = data.get("request_id").and_then(Value::as_str) else {
            continue;
        };
        match kind {
            "merge_cleanup_requested" => {
                let Some(request_repo) = data.get("repo").and_then(Value::as_str) else {
                    continue;
                };
                let Some(pr) = data.get("pr").and_then(Value::as_i64) else {
                    continue;
                };
                let ts_unix = event
                    .get("ts")
                    .and_then(Value::as_str)
                    .and_then(crate::tick_ledger::parse_rfc3339_unix)
                    .map(|v| v as i64)
                    .unwrap_or(0);
                let merged_at = data
                    .get("merged_at")
                    .and_then(Value::as_str)
                    .and_then(crate::tick_ledger::parse_rfc3339_unix)
                    .map(|v| v as i64);
                let string_field = |key: &str| {
                    data.get(key)
                        .and_then(Value::as_str)
                        .filter(|v| !v.is_empty())
                        .map(str::to_owned)
                };
                let strings = |key: &str| {
                    data.get(key)
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                };
                let mint = MergeCleanupRequest {
                    request_id: request_id.to_owned(),
                    repo: request_repo.to_owned(),
                    pr,
                    branch: string_field("branch"),
                    worktree: string_field("worktree"),
                    node_ids: strings("node_ids"),
                    candidate_row_names: strings("candidate_row_names"),
                    merged_at,
                    ts_unix,
                    session_id: string_field("session_id"),
                    harness: string_field("harness"),
                };
                match requested.remove(request_id) {
                    Some(kept) => {
                        requested.insert(request_id.to_owned(), fold_request(kept, mint));
                    }
                    None => {
                        requested.insert(request_id.to_owned(), mint);
                    }
                }
            }
            "merge_cleanup_completed" | "merge_cleanup_refused" | "merge_cleanup_expired" => {
                finished.insert(request_id.to_owned());
            }
            _ => {}
        }
    }
    (requested, finished)
}

/// Fold a second envelope for a shared request id into the kept one: the
/// union of node_ids, the first non-empty repo/worktree/branch/session_id/
/// harness, the earliest merged_at and ts. Last-write-wins would let a
/// ritual mint carrying `worktree: null` erase the merge mint's tree.
fn fold_request(mut kept: MergeCleanupRequest, next: MergeCleanupRequest) -> MergeCleanupRequest {
    for id in next.node_ids {
        if !kept.node_ids.contains(&id) {
            kept.node_ids.push(id);
        }
    }
    if kept.repo.is_empty() {
        kept.repo = next.repo;
    }
    for (slot, value) in [
        (&mut kept.worktree, next.worktree),
        (&mut kept.branch, next.branch),
        (&mut kept.session_id, next.session_id),
        (&mut kept.harness, next.harness),
    ] {
        if slot.is_none() {
            *slot = value;
        }
    }
    kept.merged_at = match (kept.merged_at, next.merged_at) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    kept.ts_unix = kept.ts_unix.min(next.ts_unix);
    kept
}

/// True while any pending request stands for the repo: the worktree sweep's
/// apply gate (a standing request means the pass may apply, not just report).
pub(crate) fn merge_cleanup_requested(home: &AgentsHome, repo: &str) -> bool {
    pending_merge_cleanup_requests_all(home)
        .iter()
        .any(|request| request.repo == repo)
}

/// The rows this request may remove: registry rows whose cwd IS the merged
/// worktree, whose name is an exact candidate the merge producer proposed
/// (`candidate_row_names` - the reaper never re-derives a name the
/// producer did not propose, so a prefix can never widen the removal), or
/// whose name resolves to one of the closed nodes through the shared
/// `name_route` vocabulary, so the operator's `t-`/`bp-`/`king-`/`target-`
/// worker names all join (a `target-{node}-` literal would see 5 of 29
/// rows). Sorted by name. A request-named row the registry no longer
/// carries is ALREADY gone, so it is not a candidate: a re-pass after a
/// held tree must read the row set as settled, not wedged.
fn merge_cleanup_rows(
    home: &AgentsHome,
    request: &MergeCleanupRequest,
) -> Vec<state::RegistryEntry> {
    let Ok(registry) = state::load_registry(&home.registry_json()) else {
        return Vec::new();
    };
    let ids: HashSet<String> = request.node_ids.iter().cloned().collect();
    let candidates: HashSet<String> = request.candidate_row_names.iter().cloned().collect();
    let mut rows: Vec<state::RegistryEntry> = registry
        .entries
        .into_iter()
        .filter(|entry| {
            request
                .worktree
                .as_deref()
                .is_some_and(|worktree| entry.cwd == worktree)
                || candidates.contains(&entry.name)
                || crate::node_route::name_route(&entry.name, &ids).is_some()
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows.dedup_by(|a, b| a.name == b.name);
    rows
}

/// Stop the row's harness before the registry row drops. `Some(short)` is the
/// confirmed stop (empty for a pane row, whose pane `fno agents rm` already
/// kills); `None` refuses and keeps the row for the next pass. `agents` is
/// the pass-level snapshot: read at most once per reaper pass, on the first
/// claude row that reaches this seam.
fn stop_harness_confirmed(
    home: &AgentsHome,
    entry: &state::RegistryEntry,
    agents: &crate::claude_roster::ClaudeAgentsSnapshot,
) -> Result<String, &'static str> {
    // Positive death evidence first: a finished claude agent never leaves the
    // roster, so its stop can never be confirmed by absence. The evidence
    // instrument is claude's roster, so only a claude background thread
    // consults it - every other row's process is ended by the shared stage
    // below (law d-81c6da7e).
    if crate::gc_native::stop_precedes_removal(entry)
        && crate::gc_sweep::claude_death_reason(entry, agents).is_some()
    {
        return Ok(row_stop_short(entry).unwrap_or_default());
    }
    match crate::gc_sweep::stop_row_process(home, entry) {
        true => Ok(row_stop_short(entry).unwrap_or_default()),
        false => Err("stop_refused"),
    }
}

/// The short id the stop event names: the row's transport key, else the
/// roster's resolution of the recorded harness session id (the terminal-stop
/// sweep's same ladder).
fn row_stop_short(entry: &state::RegistryEntry) -> Option<String> {
    if let Some(short) = entry.transport_short() {
        return Some(short.to_string());
    }
    let sid = entry.harness_session_id.as_deref()?.trim();
    if sid.is_empty() {
        return None;
    }
    let roster = crate::claude_roster::ClaudeRoster::load_default().ok()?;
    roster.find(sid).map(|w| w.short_id().to_string())
}

/// Forced removal from inside the leaf (git allows it), then a prune in the
/// canonical checkout. The branch is never deleted: `git worktree remove`
/// does not touch refs, and the branch is the recovery path.
/// The reclaim helper ships with the PLUGIN, not the managed project: a
/// foreign project's repo root has no scripts/lib, and the ignored failure
fn remove_tree(worktree: &str, repo_root: &str) -> bool {
    // Reclaim the build hash dir in-process while the workspace manifest can
    // still answer: the daemon carries no cargo on its PATH, so the old bash
    // shell-out deleted nothing on any merge. The same flock guard
    // and base checks as the sweep apply; the sweep reaps whatever an
    // unreadable resolution leaves behind.
    let _ = crate::cargo_build_dirs::remove_for(std::path::Path::new(worktree));
    let removed = std::process::Command::new("git")
        .current_dir(worktree)
        .args(["worktree", "remove", "--force", worktree])
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if removed {
        let _ = std::process::Command::new("git")
            .current_dir(repo_root)
            .args(["worktree", "prune"])
            .status();
    }
    removed
}

fn hold_stamp_path(home: &AgentsHome, request_id: &str) -> std::path::PathBuf {
    home.root().join(format!("merge-cleanup-hold.{request_id}"))
}

/// Emit the hold event at most once per hour per request: the stamp is the
/// last echo, and it dies with the request (completion removes it).
fn emit_hold_once_per_hour(
    home: &AgentsHome,
    emitter: &EventEmitter,
    request: &MergeCleanupRequest,
    reason: &str,
    now: i64,
) {
    let stamp = hold_stamp_path(home, &request.request_id);
    // The stamp holds `<echo ts> <reason>`: the reason is what an expiry
    // names as `last_hold`, and old bare-timestamp stamps still parse
    // (first whitespace token).
    let raw = std::fs::read_to_string(&stamp).unwrap_or_default();
    let mut stamp_parts = raw.trim().splitn(2, char::is_whitespace);
    let last_echo = stamp_parts
        .next()
        .and_then(|t| t.parse::<i64>().ok())
        .unwrap_or(0);
    let last_reason = stamp_parts.next().unwrap_or("").trim().to_string();
    if now.saturating_sub(last_echo) < MERGE_REAP_HOLD_ECHO_SECS {
        // In-window hold: the echo clock is preserved, the reason stays
        // current (holds escalate; a stale reason would misname an expiry).
        if last_reason != reason {
            let _ = std::fs::write(&stamp, format!("{last_echo} {reason}"));
        }
        return;
    }
    let _ = std::fs::write(&stamp, format!("{now} {reason}"));
    let _ = emitter.emit(
        "merge_cleanup_held",
        &json!({
            "request_id": request.request_id,
            "repo": request.repo,
            "pr": request.pr,
            "reason": reason,
        }),
    );
}

/// The per-request side effects, injected so the ORDER test can drive the
/// stop -> rm -> tree sequence without shelling. Production wiring is in
/// [`consume_merge_cleanup_requests`].
struct RequestSeams<'a> {
    /// The three finished witnesses for one row (transcript past grace, pid
    /// gone, roster terminal). `false` keeps the row: a live and working
    /// worker stays even when its node is done.
    finished: &'a dyn Fn(&state::RegistryEntry) -> bool,
    /// Stop the row's harness. `Ok(short)` names the stopped session (empty
    /// for a pane row); `Err` holds the row for this pass.
    stop: &'a dyn Fn(&state::RegistryEntry) -> Result<String, &'static str>,
    /// The native ACTIVE-SURFACE removal, typed: claude's agent list, codex's
    /// session index, cursor-agent's worker servers.
    surface_removal: &'a dyn Fn(&state::RegistryEntry) -> crate::daemon::CascadeOutcome,
    /// The mux squad-member retirement, typed: `fno mux
    /// retire-session` for the row's live squad membership.
    mux_member: &'a dyn Fn(&state::RegistryEntry) -> crate::daemon::CascadeOutcome,
    /// A live process whose cwd is inside the tree, or an unreadable cwd probe.
    tree_busy: &'a dyn Fn(&str) -> Result<bool, ()>,
    /// Forced tree removal; true = gone (the caller emits and prunes).
    take_tree: &'a dyn Fn(&str, &str) -> bool,
}

/// Settle one request past its grace window. Returns `(acted, held)`: the
/// acted count (rows removed + trees removed) and whether the request stays
/// pending. Every step names itself in events.jsonl IN ORDER:
/// merge_reaper_stopped, agent_row_reaped (emitted by the shared commit),
/// worktree_removed, merge_cleanup_completed.
fn run_request(
    home: &AgentsHome,
    emitter: &EventEmitter,
    request: &MergeCleanupRequest,
    root: &str,
    states: Option<&HashMap<String, (String, Option<String>, usize)>>,
    ledger: Option<&[Value]>,
    now: i64,
    seams: &RequestSeams,
    precomputed: Option<&HashMap<String, Vec<state::RegistryEntry>>>,
) -> (u64, bool) {
    // 2. Doneness re-read: every named node must read done, with no recorded
    // merge_status that contradicts the merge. An ABSENT merge_status passes
    // (the gc sweep's rule: unrecorded is not a contradiction - both mint
    // sites only fire against a gh-confirmed MERGED state, and cascade closes
    // leave the field null). A recorded non-merged value holds under its own
    // reason; a node not done, or unknown to the graph, still holds. An EMPTY
    // node list holds too - no proof, no removal. A graph that will not read
    // holds for the same reason.
    let Some(states) = states else {
        emit_hold_once_per_hour(home, emitter, request, "graph-unreadable", now);
        return (0, true);
    };
    let open: Vec<String> = request
        .node_ids
        .iter()
        .filter_map(|id| match states.get(id.as_str()) {
            Some((status, merge_status, additional_open)) if status == "done" => {
                match merge_status.as_deref() {
                    Some(m) if m != "merged" => Some(format!("merge-status:{m}:{id}")),
                    _ if *additional_open > 0 => Some(format!("additional-pr-open:{id}")),
                    _ => None,
                }
            }
            _ => Some(format!("node-open:{id}")),
        })
        .collect();
    if request.node_ids.is_empty() || !open.is_empty() {
        let reason = match open.first() {
            Some(id) => format!("node-open:{id}"),
            None => "no-node-ids".to_string(),
        };
        emit_hold_once_per_hour(home, emitter, request, &reason, now);
        return (0, true);
    }
    // 3. Candidates, with the crowned and operator-origin rows named out: an
    // idle king reads state=done, so exclusion is by NAME, never by roster
    // state. Crowned/operator rows are settled keeps; a refusal below is a
    // HOLD - the request matched the row and must come back for it.
    // The pass precomputed these exact candidates for the batched age
    // probe; reuse them so the registry is read once per request, not twice.
    let joined = precomputed
        .and_then(|m| m.get(&request.request_id))
        .cloned()
        .unwrap_or_else(|| merge_cleanup_rows(home, request));
    let mut kept: Vec<String> = Vec::new();
    let mut rows: Vec<state::RegistryEntry> = Vec::new();
    for entry in joined {
        if entry.crown_level.is_some() {
            kept.push(format!("{}:kept_crowned", entry.name));
            continue;
        }
        if entry.origin.as_deref() == Some("operator") {
            kept.push(format!("{}:kept_operator", entry.name));
            continue;
        }
        rows.push(entry);
    }
    // 4+5. The session half of every candidate runs through the SAME sequence
    // the scheduled sweep runs - stop, native active-surface removal, the
    // resumable receipt - and one registry write drops the staged rows under
    // that sweep's `created_at` guard. The merge path keeps only what is its
    // own: the trigger above and the tree below. A stop or receipt refusal
    // keeps its own row; the rest of the request still settles.
    let mut receipts = BTreeMap::new();
    let mut to_retire = BTreeMap::new();
    // Refusals are holds, not completion-kept rows: the whole request comes
    // back for them, because a completion tombstones it and the merge
    // trigger never returns.
    let mut held_rows: Vec<String> = Vec::new();
    // The merge path names its own stop in the journal: `merge_reaper_stopped`
    // is what puts the stop BEFORE the row drop in a reader's hands.
    let stop = |entry: &state::RegistryEntry| match (seams.stop)(entry) {
        Ok(short) => {
            if !short.is_empty() {
                let _ = emitter.emit(
                    "merge_reaper_stopped",
                    &json!({
                        "short_id": short,
                        "name": entry.name,
                        "request_id": request.request_id,
                        "harness": entry.harness_name(),
                    }),
                );
            }
            true
        }
        Err(_) => false,
    };
    for entry in &rows {
        // The finished gate BEFORE any stop: a row that fails it stays, and
        // the completion is never reached, so the request retries it.
        if !(seams.finished)(entry) {
            held_rows.push(format!("{}:still_writing", entry.name));
            continue;
        }
        match crate::gc_sweep::stage_session_retirement(
            home,
            entry,
            ledger,
            crate::gc_sweep::RetireMode::Apply,
            // The observation spares only a rehearsal a mutation; apply
            // reads the stop gate from the stop itself.
            crate::gc_sweep::StopObservation::Unproven,
            false,
            &stop,
            &crate::pane_stop::run_mux_pane_kill,
            seams.surface_removal,
            seams.mux_member,
            &mut receipts,
        ) {
            Ok(crate::gc_sweep::StagedRetirement::Retired) => {
                to_retire.insert(
                    entry.name.clone(),
                    crate::gc_sweep::RetireOrder {
                        id: entry.name.clone(),
                        released: false,
                        via_release: false,
                        basis: format!(
                            "merge-cleanup:{} all nodes done+merged",
                            request.request_id
                        ),
                        created_at: entry.created_at.clone(),
                        // The tree is this path's own authority (step 6),
                        // under a done-and-merged rule the sweep's
                        // clean-and-merged prune does not carry. The shared
                        // commit never touches it.
                        tree: crate::gc::TreeAction::None,
                        worktree: None,
                    },
                );
            }
            // apply cannot produce Unverified; if a future path ever
            // does, the row is kept and named, never retired on it.
            Ok(crate::gc_sweep::StagedRetirement::Unverified(_)) => {
                held_rows.push(format!("{}:dry_run_unverified", entry.name));
            }
            Err(refusal) => held_rows.push(format!(
                "{name}:{reason}",
                name = entry.name,
                reason = match refusal {
                    crate::gc_sweep::RetireRefusal::StopRefused(_) => "stop_refused",
                    crate::gc_sweep::RetireRefusal::NativeRemoval(_) =>
                        "native_removal_unconfirmed",
                    crate::gc_sweep::RetireRefusal::NoReceipt(_) => "no_receipt",
                    crate::gc_sweep::RetireRefusal::GraphObligation(_) => "open_do_row",
                    // Both unreachable in apply mode; the match is
                    // exhaustive so a future mode leak cannot retire a row.
                    crate::gc_sweep::RetireRefusal::StopUnproven(_) => "stop_unproven",
                    crate::gc_sweep::RetireRefusal::GraphUnreadable => "graph_unreadable",
                }
            )),
        }
    }
    let report = crate::gc_sweep::commit_retirements(
        home,
        emitter,
        "merge_reap",
        &rows,
        &mut to_retire,
        &receipts,
        &|_| None,
    );
    kept.extend(
        report
            .kept_no_receipt
            .iter()
            .map(|(name, reason)| format!("{name}:{reason}")),
    );
    // A row staged but not removed is still live, whether the registry write
    // failed or the `created_at` guard found a replacement session owning the
    // name. Either way the tree must not go this pass, and the reason says
    // what is true of both: the row is not gone.
    if let Some(name) = to_retire
        .keys()
        .find(|name| !report.retired_names.contains(*name))
    {
        let _ = emitter.emit(
            "merge_cleanup_refused",
            &json!({
                "request_id": request.request_id,
                "repo": request.repo,
                "pr": request.pr,
                "reason": format!("row-not-removed:{name}"),
            }),
        );
        return (0, true);
    }
    // The request matched rows it did not finish (still writing, stop
    // refused, an unverified effect): hold instead of completing, so a
    // later pass returns to them. Rows already removed stay removed; the
    // tree waits with the held rows.
    if !held_rows.is_empty() {
        emit_hold_once_per_hour(
            home,
            emitter,
            request,
            &format!("rows-kept:{}", held_rows.join(",")),
            now,
        );
        return (report.retired_names.len() as u64, true);
    }
    let removed_rows: Vec<String> = report.retired_names.iter().cloned().collect();
    // 6. The tree, after the rows: whatever its git status, a done and
    // merged node's tree goes. A live process cwd is the remaining hold.
    let mut reclaimed_bytes: u64 = 0;
    let mut tree_note = "no-worktree";
    if let Some(worktree) = request.worktree.as_deref() {
        if std::path::Path::new(worktree).exists() {
            match (seams.tree_busy)(worktree) {
                Ok(true) => {
                    emit_hold_once_per_hour(home, emitter, request, "tree-held:process-cwd", now);
                    return (removed_rows.len() as u64, true);
                }
                Err(()) => {
                    emit_hold_once_per_hour(
                        home,
                        emitter,
                        request,
                        "tree-held:cwd-unreadable",
                        now,
                    );
                    return (removed_rows.len() as u64, true);
                }
                Ok(false) => {}
            }
            reclaimed_bytes =
                crate::daemon::directory_bytes(std::path::Path::new(worktree)).unwrap_or(0);
            if (seams.take_tree)(worktree, root) {
                let _ = emitter.emit(
                    "worktree_removed",
                    &json!({
                        "path": worktree,
                        "caller": "merge-reaper",
                        "claim": format!(
                            "merge-cleanup:{} all nodes done+merged",
                            request.request_id
                        ),
                        "reason": "pr-merged; dirty is not a hold for a done node",
                        "branch": request.branch,
                        "forced": true,
                        "reclaimed_bytes": reclaimed_bytes,
                    }),
                );
                tree_note = "removed";
            } else {
                // A transient git failure retries on a later pass; tombstoning
                // here would strand the tree the same way a hold would.
                emit_hold_once_per_hour(home, emitter, request, "tree-held:removal-failed", now);
                return (removed_rows.len() as u64, true);
            }
        }
    }
    let _ = emitter.emit(
        "merge_cleanup_completed",
        &json!({
            "request_id": request.request_id,
            "repo": request.repo,
            "pr": request.pr,
            "reclaimed_bytes": reclaimed_bytes,
            "removed_rows": removed_rows,
            "kept": kept,
            // The positive marker for a zero-row completion: "none-present"
            // says the join found no row for this request's nodes, so the
            // tombstone is honest. An empty removed_rows alone read both
            // ways, and 89 of 117 completions read as held.
            "rows": if rows.is_empty() && kept.is_empty() {
                "none-present"
            } else {
                "present"
            },
            "tree": tree_note,
        }),
    );
    let _ = std::fs::remove_file(hold_stamp_path(home, &request.request_id));
    (
        removed_rows.len() as u64 + u64::from(tree_note == "removed"),
        false,
    )
}

/// One pass: every pending request, in grace/doneness/stop/rm/tree order.
/// Sync by design - the daemon runs it inside the worktree sweep's
/// `spawn_blocking` task, behind the same one-in-flight gate.
pub(crate) fn consume_merge_cleanup_requests(
    home: &AgentsHome,
    roots: &[String],
    emitter: &EventEmitter,
    grace_secs: i64,
) {
    let stamp = home.root().join("merge-reap.stamp");
    let now = crate::daemon::now_epoch_secs();
    let last = std::fs::read_to_string(&stamp)
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0);
    if now.saturating_sub(last) < MERGE_REAP_INTERVAL_SECS as i64 {
        return;
    }
    let _ = std::fs::write(&stamp, now.to_string());

    // One doneness read per pass, not per request, and ONE journal read per
    // pass partitioned in memory: N repo roots cost one fold, not N. The
    // ledger the receipts enrich from is read on the same terms.
    let node_states = crate::gc_sweep::read_graph_node_states(home);
    let ledger = crate::gc_sweep::ledger_rows(&crate::gc_sweep::default_ledger_path());
    let mut pending = pending_merge_cleanup_requests_all(home);
    // The merge ends each node's ship rows at the instant the merge
    // recorded, grace or not. A request with no merged_at waits for the
    // sweep, which reads the merge commit instead.
    let ship_ends: Vec<(String, String)> = pending
        .iter()
        .filter_map(|r| Some((r, chrono::DateTime::from_timestamp(r.merged_at?, 0)?)))
        .flat_map(|(r, at)| {
            let at = at.format("%Y-%m-%dT%H:%M:%SZ").to_string();
            r.node_ids.iter().map(move |id| (id.clone(), at.clone()))
        })
        .collect();
    let store = crate::backlog::api::Store::new(&crate::gc_sweep::graph_path(home));
    for (node, error) in crate::phase_close::close_ship_rows_at(&store, &ship_ends, "merge") {
        let _ = emitter.emit(
            "daemon_recovery_error",
            &json!({"op": "close_ship_rows", "node": node, "error": error}),
        );
    }
    let worktrees_by_root: HashMap<String, Vec<(String, std::path::PathBuf)>> = roots
        .iter()
        .map(|root| {
            (
                root.clone(),
                crate::heal::worktrees_by_branch("git", std::path::Path::new(root)),
            )
        })
        .collect();
    for request in &mut pending {
        if request.worktree.is_some() {
            continue;
        }
        let Some(branch) = request.branch.as_deref() else {
            continue;
        };
        let Some(worktrees) = worktrees_by_root.get(&request.repo) else {
            continue;
        };
        if let Some((_, path)) = worktrees
            .iter()
            .find(|(name, path)| name == branch && path != std::path::Path::new(&request.repo))
        {
            request.worktree = Some(path.to_string_lossy().into_owned());
        }
    }

    let mut total_requests = 0usize;
    let mut in_grace = 0usize;
    let mut acted: u64 = 0;
    let mut held_requests = 0usize;
    // The agents snapshot is read at most once per reaper pass, on the first
    // claude row that reaches a stop seam - never rows x 15s on a degraded
    // roster.
    let agents_memo: std::cell::RefCell<Option<crate::claude_roster::ClaudeAgentsSnapshot>> =
        std::cell::RefCell::new(None);
    // change 1: the age seam batched. Every candidate row of every
    // request past the merge grace is probed in ONE child (the same seam
    // `gc_sweep::run` takes) instead of one child per row - the per-row
    // wrapper `probe_row_age` is gone. The eligibility here mirrors the
    // loop below: in-grace and expired requests never reach `finished`,
    // so their rows cost nothing to probe.
    let mut batch_entries: Vec<state::RegistryEntry> = Vec::new();
    let mut precomputed: HashMap<String, Vec<state::RegistryEntry>> = HashMap::new();
    for root in roots {
        for request in pending.iter().filter(|r| r.repo == *root) {
            let merged_at = request.merged_at.unwrap_or(request.ts_unix);
            let age = now.saturating_sub(merged_at);
            let expiry_age = now.saturating_sub(request.ts_unix.max(merged_at));
            if age < grace_secs.max(0) || expiry_age > MERGE_REAP_EXPIRY_SECS {
                continue;
            }
            let mut rows = Vec::new();
            for entry in merge_cleanup_rows(home, request) {
                if entry.crown_level.is_some() || entry.origin.as_deref() == Some("operator") {
                    continue;
                }
                batch_entries.push(entry.clone());
                rows.push(entry);
            }
            precomputed.insert(request.request_id.clone(), rows);
        }
    }
    let batch_refs: Vec<&state::RegistryEntry> = batch_entries.iter().collect();
    let ages = crate::gc::probe_entry_ages(&batch_refs);
    for root in roots {
        for request in pending.iter().filter(|r| r.repo == *root) {
            total_requests += 1;
            let merged_at = request.merged_at.unwrap_or(request.ts_unix);
            let age = now.saturating_sub(merged_at);
            let expiry_age = now.saturating_sub(request.ts_unix.max(merged_at));
            // 1. Grace: a clock from the merge moment, never a liveness
            // probe. A session stopped inside the window stays resumable
            // through its transcript, which is what makes the wait safe.
            if age < grace_secs.max(0) {
                in_grace += 1;
                continue;
            }
            if expiry_age > MERGE_REAP_EXPIRY_SECS {
                // The last hold names itself in the expiry, so a benign
                // expiry (no worktree ever held) stops reading like one that
                // stranded a real tree. Read before the tombstone removes it.
                let last_hold = std::fs::read_to_string(hold_stamp_path(home, &request.request_id))
                    .ok()
                    .and_then(|raw| {
                        raw.trim()
                            .splitn(2, char::is_whitespace)
                            .nth(1)
                            .map(|r| r.trim().to_string())
                            .filter(|r| !r.is_empty())
                    })
                    .unwrap_or_else(|| "none".to_string());
                let _ = emitter.emit(
                    "merge_cleanup_expired",
                    &json!({
                        "request_id": request.request_id,
                        "repo": request.repo,
                        "pr": request.pr,
                        "reason": "expired",
                        "last_hold": last_hold,
                    }),
                );
                let _ = std::fs::remove_file(hold_stamp_path(home, &request.request_id));
                continue;
            }
            let seams = RequestSeams {
                finished: &|entry| {
                    let age = ages.get(&crate::gc::row_handle(entry)).copied().flatten();
                    let terminal = if entry.harness_name() == "claude" {
                        let mut memo = agents_memo.borrow_mut();
                        let agents = memo.get_or_insert_with(crate::claude_roster::read_all_agents);
                        crate::gc_sweep::claude_death_reason(entry, agents)
                    } else {
                        None
                    };
                    crate::gc_sweep::worker_finished(entry, age, grace_secs, terminal.as_deref())
                },
                stop: &|entry| {
                    let mut memo = agents_memo.borrow_mut();
                    let agents = memo.get_or_insert_with(crate::claude_roster::read_all_agents);
                    stop_harness_confirmed(home, entry, agents)
                },
                surface_removal: &crate::gc_native::apply_active_surface_removal,
                mux_member: &crate::gc_native::apply_mux_member_retirement,
                tree_busy: &|worktree| {
                    let target = std::path::PathBuf::from(worktree);
                    let canonical_target =
                        std::fs::canonicalize(&target).unwrap_or_else(|_| target.clone());
                    let cwds = crate::cargo_build_dirs::live_cwds(None)?;
                    Ok(cwds.into_iter().any(|cwd| {
                        let canonical_cwd =
                            std::fs::canonicalize(&cwd).unwrap_or_else(|_| cwd.clone());
                        cwd.starts_with(&target) || canonical_cwd.starts_with(&canonical_target)
                    }))
                },
                take_tree: &remove_tree,
            };
            let (acted_n, held) = run_request(
                home,
                emitter,
                request,
                root,
                node_states.as_ref(),
                ledger.as_deref(),
                now,
                &seams,
                Some(&precomputed),
            );
            acted += acted_n;
            held_requests += usize::from(held);
        }
    }

    let skip_reason = if total_requests == 0 {
        Some("no_requests")
    } else if total_requests == in_grace {
        Some("all_in_grace")
    } else if acted == 0 {
        Some("held")
    } else {
        None
    };
    let journal = crate::loop_runtime::Journal::new_raw(
        home.events_jsonl(),
        crate::daemon::global_events_path(home),
    );
    crate::tick_ledger::emit_tick(
        &journal,
        "reap",
        "daemon",
        acted,
        skip_reason,
        Some(&format!("requests={total_requests} held={held_requests}")),
        MERGE_REAP_INTERVAL_SECS,
    );
}

/// The pending set for one repo: every request minus the ones a tombstone
/// already settled. `merge_cleanup_completed` / `_refused` / `_expired` all
/// finish a request; a held request stays pending and is re-read every pass.
/// One journal read per call; the reaper pass calls the `_all` variant once
/// and partitions in memory, so N roots cost one read, not N.
#[cfg(test)]
pub(crate) fn pending_merge_cleanup_requests(
    home: &AgentsHome,
    repo: &str,
) -> Vec<MergeCleanupRequest> {
    pending_merge_cleanup_requests_all(home)
        .into_iter()
        .filter(|request| request.repo == repo)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    fn stamps() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    /// The home root sits one level below its own parent, so the tick
    /// journal's global mirror lands inside this test's temp tree too.
    fn temp_home(tag: &str) -> AgentsHome {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "fno-merge-reap-{tag}-{}-{}",
            std::process::id(),
            stamps()
        ));
        let home_dir = p.join("home");
        std::fs::create_dir_all(&home_dir).unwrap();
        AgentsHome::at(&home_dir)
    }

    fn request_line(ts: &str, id: &str, merged_at: Option<&str>, nodes: Value) -> String {
        let mut data = json!({
            "request_id": id,
            "repo": "/repo",
            "pr": 42,
            "branch": "feature/x",
            "worktree": "/repo/wt",
            "node_ids": nodes,
        });
        if let Some(at) = merged_at {
            data["merged_at"] = json!(at);
        }
        json!({
            "ts": ts,
            "type": "merge_cleanup_requested",
            "source": "python",
            "data": data,
        })
        .to_string()
    }

    fn write_events(home: &AgentsHome, lines: &[String]) {
        std::fs::create_dir_all(home.root()).unwrap();
        std::fs::write(home.events_jsonl(), lines.join("\n") + "\n").unwrap();
    }

    #[test]
    fn fold_keeps_requests_until_a_tombstone_settles_them() {
        let home = temp_home("fold");
        write_events(
            &home,
            &[request_line(
                "2026-09-06T00:00:00Z",
                "merge-cleanup-1",
                None,
                json!(["x-1"]),
            )],
        );
        assert_eq!(pending_merge_cleanup_requests(&home, "/repo").len(), 1);
        assert!(merge_cleanup_requested(&home, "/repo"));

        let completed = json!({
            "ts": "2026-09-06T01:00:00Z",
            "type": "merge_cleanup_completed",
            "source": "daemon",
            "data": {"request_id": "merge-cleanup-1", "repo": "/repo", "pr": 42, "reclaimed_bytes": 12}
        });
        std::fs::OpenOptions::new()
            .append(true)
            .open(home.events_jsonl())
            .unwrap()
            .write_all(format!("{}\n", completed).as_bytes())
            .unwrap();
        assert!(pending_merge_cleanup_requests(&home, "/repo").is_empty());
        assert!(!merge_cleanup_requested(&home, "/repo"));
        std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
    }

    #[test]
    fn expired_tombstone_also_settles_a_request() {
        let home = temp_home("expired");
        write_events(
            &home,
            &[request_line(
                "2026-09-06T00:00:00Z",
                "merge-cleanup-1",
                None,
                json!(["x-1"]),
            )],
        );
        let expired = json!({
            "ts": "2026-09-07T00:00:00Z",
            "type": "merge_cleanup_expired",
            "source": "daemon",
            "data": {"request_id": "merge-cleanup-1", "repo": "/repo", "pr": 42, "reason": "expired"}
        });
        std::fs::OpenOptions::new()
            .append(true)
            .open(home.events_jsonl())
            .unwrap()
            .write_all(format!("{}\n", expired).as_bytes())
            .unwrap();
        assert!(pending_merge_cleanup_requests(&home, "/repo").is_empty());
        std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
    }

    #[test]
    fn pending_read_spans_one_rotation() {
        // AC2-HP: a request that rotated into the .1 generation stays pending,
        // and a tombstone in the active file still settles it.
        let home = temp_home("rotation-span");
        let rotated = std::path::PathBuf::from(format!("{}.1", home.events_jsonl().display()));
        std::fs::create_dir_all(home.root()).unwrap();
        std::fs::write(
            &rotated,
            request_line(
                "2026-09-06T00:00:00Z",
                "merge-cleanup-1",
                None,
                json!(["x-1"]),
            ) + "\n",
        )
        .unwrap();
        write_events(
            &home,
            &[request_line(
                "2026-09-06T01:00:00Z",
                "merge-cleanup-2",
                None,
                json!(["x-2"]),
            )],
        );
        // Production rotation ingests a generation before the rename, so the
        // store already holds the rotated row when the reader runs.
        crate::event_store::sync(&home.events_jsonl()).unwrap();
        let ids: Vec<String> = pending_merge_cleanup_requests_all(&home)
            .into_iter()
            .map(|r| r.request_id)
            .collect();
        assert!(ids.contains(&"merge-cleanup-1".to_string()), "{ids:?}");
        assert!(ids.contains(&"merge-cleanup-2".to_string()), "{ids:?}");

        let completed = json!({
            "ts": "2026-09-06T02:00:00Z",
            "type": "merge_cleanup_completed",
            "source": "daemon",
            "data": {"request_id": "merge-cleanup-1", "repo": "/repo", "pr": 42}
        });
        std::fs::OpenOptions::new()
            .append(true)
            .open(home.events_jsonl())
            .unwrap()
            .write_all(format!("{}\n", completed).as_bytes())
            .unwrap();
        let ids: Vec<String> = pending_merge_cleanup_requests_all(&home)
            .into_iter()
            .map(|r| r.request_id)
            .collect();
        assert_eq!(ids, vec!["merge-cleanup-2".to_string()]);
        std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
    }

    #[test]
    fn pending_read_sees_store_committed_requests() {
        // AC3-HP: a request and its completion committed to the store only.
        let home = temp_home("store-committed");
        std::fs::create_dir_all(home.root()).unwrap();
        let requested = request_line("2026-09-06T00:00:00Z", "store-req-1", None, json!(["x-1"]));
        crate::event_store::append_envelope(&home.events_jsonl(), &requested, None).unwrap();
        assert_eq!(
            pending_merge_cleanup_requests(&home, "/repo").len(),
            1,
            "the request alone is pending"
        );
        let completed = json!({
            "ts": "2026-09-06T02:00:00Z",
            "type": "merge_cleanup_completed",
            "source": "daemon",
            "data": {"request_id": "store-req-1", "repo": "/repo", "pr": 42}
        })
        .to_string();
        crate::event_store::append_envelope(&home.events_jsonl(), &completed, None).unwrap();
        assert!(
            pending_merge_cleanup_requests(&home, "/repo").is_empty(),
            "the store-only tombstone settles the store-only request"
        );
        std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
    }

    #[test]
    fn duplicate_mints_fold_field_by_field() {
        // AC2-FOLD: the merge mint (worktree set, empty ids, earlier merged_at)
        // and the ritual mint (worktree null, ids named) share one request id;
        // the fold keeps the union, the first non-empty worktree and the
        // earliest merged_at, instead of the last envelope winning.
        let home = temp_home("fold-dup");
        let merge_mint = json!({
            "ts": "2026-09-10T11:00:00Z",
            "type": "merge_cleanup_requested",
            "source": "python",
            "data": {
                "request_id": "merge-cleanup-1",
                "repo": "/repo",
                "pr": 42,
                "branch": "feature/x",
                "worktree": "/repo/wt",
                "node_ids": [],
                "merged_at": "2026-09-10T11:00:00Z",
            }
        })
        .to_string();
        let ritual_mint = json!({
            "ts": "2026-09-10T11:05:00Z",
            "type": "merge_cleanup_requested",
            "source": "python",
            "data": {
                "request_id": "merge-cleanup-1",
                "repo": "/repo",
                "pr": 42,
                "branch": "feature/x",
                "worktree": null,
                "node_ids": ["x-1"],
                "merged_at": "2026-09-10T11:05:00Z",
            }
        })
        .to_string();
        write_events(&home, &[merge_mint, ritual_mint]);
        let pending = pending_merge_cleanup_requests_all(&home);
        assert_eq!(pending.len(), 1);
        let request = &pending[0];
        assert_eq!(request.worktree.as_deref(), Some("/repo/wt"));
        assert_eq!(request.node_ids, vec!["x-1".to_string()]);
        assert_eq!(
            request.merged_at,
            crate::tick_ledger::parse_rfc3339_unix("2026-09-10T11:00:00Z").map(|v| v as i64)
        );
        std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
    }

    #[test]
    fn requested_repos_span_the_rotated_generation() {
        // AC2-ROOTS: a repo whose only request rotated into .1 stays in the
        // reaper's roots (its worktree sweep keeps the apply gate).
        let home = temp_home("roots");
        let repo_dir = home.root().parent().unwrap().join("repo-root");
        std::fs::create_dir_all(&repo_dir).unwrap();
        let line = json!({
            "ts": "2026-09-06T00:00:00Z",
            "type": "merge_cleanup_requested",
            "source": "python",
            "data": {
                "request_id": "merge-cleanup-1",
                "repo": repo_dir.display().to_string(),
                "pr": 42,
                "branch": "feature/x",
                "node_ids": ["x-1"],
            }
        })
        .to_string();
        let rotated = std::path::PathBuf::from(format!("{}.1", home.events_jsonl().display()));
        std::fs::create_dir_all(home.root()).unwrap();
        std::fs::write(&rotated, line + "\n").unwrap();
        // Production rotation ingests a generation before the rename.
        crate::event_store::sync(&home.events_jsonl()).unwrap();
        assert!(merge_cleanup_request_repos(&home).contains(&repo_dir.display().to_string()));
        std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
    }

    #[test]
    fn absent_merge_status_does_not_hold_a_done_node() {
        // AC3-HP: an unrecorded merge_status is not a contradiction; a done
        // node passes the doneness read and the request settles.
        let home = temp_home("null-merge-status");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        write_registry(&home, &[]);
        let mut states = HashMap::new();
        states.insert("x-1".to_string(), ("done".to_string(), None, 0));
        states.insert(
            "x-2".to_string(),
            ("done".to_string(), Some("merged".to_string()), 0),
        );
        let mut request = settled_request("/repo/wt");
        request.worktree = None;
        request.node_ids = vec!["x-1".to_string(), "x-2".to_string()];
        let seams = RequestSeams {
            finished: &|_entry| true,
            stop: &|_entry| Ok("abc123".to_string()),
            surface_removal: &|_entry| crate::daemon::CascadeOutcome::Removed,
            mux_member: &|_entry| crate::daemon::CascadeOutcome::NotApplicable,
            take_tree: &|_wt, _root| true,
            tree_busy: &|_wt| Ok(false),
        };
        let (acted, held) = run_request(
            &home,
            &emitter,
            &request,
            "/repo",
            Some(&states),
            None,
            1_000_000,
            &seams,
            None,
        );
        assert_eq!(acted, 0);
        assert!(
            !held,
            "no recorded merge_status passes the doneness re-read"
        );
        let events = crate::events::committed_journal_text(&home.events_jsonl());
        assert!(
            events.contains("\"type\":\"merge_cleanup_completed\""),
            "the request must settle: {events}"
        );
        std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
    }

    #[test]
    fn recorded_non_merged_merge_status_holds_under_its_own_reason() {
        // AC3-ERR: a done node whose merge_status is recorded and not
        // `merged` holds naming merge-status:<value>:<id>, not node-open.
        let home = temp_home("merge-status-hold");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        write_registry(&home, &[]);
        let mut states = HashMap::new();
        states.insert(
            "x-1".to_string(),
            ("done".to_string(), Some("open".to_string()), 0),
        );
        let mut request = settled_request("/repo/wt");
        request.worktree = None;
        let seams = RequestSeams {
            finished: &|_entry| true,
            stop: &|_entry| Ok("abc123".to_string()),
            surface_removal: &|_entry| crate::daemon::CascadeOutcome::Removed,
            mux_member: &|_entry| crate::daemon::CascadeOutcome::NotApplicable,
            take_tree: &|_wt, _root| true,
            tree_busy: &|_wt| Ok(false),
        };
        let (acted, held) = run_request(
            &home,
            &emitter,
            &request,
            "/repo",
            Some(&states),
            None,
            1_000_000,
            &seams,
            None,
        );
        assert_eq!(acted, 0);
        assert!(held, "a recorded non-merged status holds the request");
        let events = crate::events::committed_journal_text(&home.events_jsonl());
        assert!(
            events.contains("merge-status:open:x-1"),
            "the hold must name the recorded merge_status: {events}"
        );
        std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
    }

    #[test]
    fn open_additional_pr_holds_the_cleanup_request() {
        // The doneness gate honors the same keep as the retire sweep: a
        // done+merged node whose additional_prs record one still open holds
        // the request under additional-pr-open:<node>; no row, no tree.
        let home = temp_home("additional-pr-open");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        write_registry(&home, &[]);
        let mut states = HashMap::new();
        states.insert(
            "x-1".to_string(),
            ("done".to_string(), Some("merged".to_string()), 1),
        );
        let mut request = settled_request("/repo/wt");
        request.worktree = None;
        let seams = RequestSeams {
            finished: &|_entry| true,
            stop: &|_entry| Ok("abc123".to_string()),
            surface_removal: &|_entry| crate::daemon::CascadeOutcome::Removed,
            mux_member: &|_entry| crate::daemon::CascadeOutcome::NotApplicable,
            take_tree: &|_wt, _root| true,
            tree_busy: &|_wt| Ok(false),
        };
        let (acted, held) = run_request(
            &home,
            &emitter,
            &request,
            "/repo",
            Some(&states),
            None,
            1_000_000,
            &seams,
            None,
        );
        assert_eq!(acted, 0);
        assert!(held, "an open additional PR holds the request");
        let events = crate::events::committed_journal_text(&home.events_jsonl());
        assert!(
            events.contains("additional-pr-open:x-1"),
            "the hold must name the open additional PR: {events}"
        );
        std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
    }

    #[test]
    fn grace_anchors_on_merged_at_not_the_envelope_ts() {
        let home = temp_home("grace-anchor");
        // The request was WRITTEN a day ago, but merged_at is seconds old:
        // the clock that matters is the merge's.
        write_events(
            &home,
            &[request_line(
                "2026-09-06T00:00:00Z",
                "merge-cleanup-1",
                Some("2026-09-07T11:59:00Z"),
                json!(["x-1"]),
            )],
        );
        let request = &pending_merge_cleanup_requests(&home, "/repo")[0];
        assert_eq!(request.merged_at, Some(1_788_782_340));
        assert_eq!(request.ts_unix, 1_788_652_800);
        std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
    }

    #[test]
    fn in_grace_pass_is_named_not_silent() {
        let home = temp_home("in-grace");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        // merged_at = now: the whole pass is inside the window.
        write_events(
            &home,
            &[request_line(
                "2026-09-06T00:00:00Z",
                "merge-cleanup-1",
                None,
                json!(["x-1"]),
            )],
        );
        // Older-than-60s stamp so the pass runs; the fresh merged_at (None ->
        // envelope ts 2026-09-06) is deep inside grace against a 900s window
        // only if now is near it, so instead pin grace huge: every request
        // reads in_grace and the tick row says so.
        std::fs::create_dir_all(home.root()).unwrap();
        std::fs::write(home.root().join("merge-reap.stamp"), "0").unwrap();
        consume_merge_cleanup_requests(&home, &["/repo".to_string()], &emitter, i64::MAX / 2);
        let events = crate::events::committed_journal_text(&home.events_jsonl());
        assert!(
            events.contains("\"skip_reason\":\"all_in_grace\""),
            "the tick row must name the grace hold: {events}"
        );
        std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
    }

    /// An expiry names the reason of its last hold, so a
    /// benign expiry (a branch with no worktree) stops reading like one that
    /// stranded a real tree.
    #[test]
    fn an_expiry_names_the_reason_of_its_last_hold() {
        let home = temp_home("expiry-hold");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        write_events(
            &home,
            &[request_line(
                "2026-09-06T00:00:00Z",
                "merge-cleanup-1",
                None,
                json!(["x-1"]),
            )],
        );
        std::fs::create_dir_all(home.root()).unwrap();
        std::fs::write(
            home.root().join("merge-cleanup-hold.merge-cleanup-1"),
            "1788652300 tree-held:unreachable-from-origin-main",
        )
        .unwrap();
        // Grace 0: the 2026-09-06 request is far past the expiry window, so
        // the pass takes the expiry branch. A huge grace (the in-grace
        // test's pin) would keep every request in the window forever.
        consume_merge_cleanup_requests(&home, &["/repo".to_string()], &emitter, 0);
        let events = crate::events::committed_journal_text(&home.events_jsonl());
        let expired = events
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .find(|v| v.get("type").and_then(Value::as_str) == Some("merge_cleanup_expired"))
            .expect("the expiry was emitted");
        assert_eq!(
            expired["data"]["last_hold"], "tree-held:unreachable-from-origin-main",
            "{expired}"
        );
        assert!(
            !home
                .root()
                .join("merge-cleanup-hold.merge-cleanup-1")
                .exists(),
            "the tombstone removed its stamp"
        );
        std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
    }

    /// The same expiry against a PRE-WIDENING stamp file (a bare timestamp,
    /// no reason) reads `last_hold: "none"`: old stamp files still parse and
    /// an unknown hold never masquerades as a named one.
    #[test]
    fn an_expiry_without_a_hold_reads_none() {
        let home = temp_home("expiry-none");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        write_events(
            &home,
            &[request_line(
                "2026-09-06T00:00:00Z",
                "merge-cleanup-1",
                None,
                json!(["x-1"]),
            )],
        );
        std::fs::create_dir_all(home.root()).unwrap();
        std::fs::write(
            home.root().join("merge-cleanup-hold.merge-cleanup-1"),
            "1788652300",
        )
        .unwrap();
        // Grace 0, as in an_expiry_names_the_reason_of_its_last_hold: the
        // request must land in the expiry branch for the stamp to be read.
        consume_merge_cleanup_requests(&home, &["/repo".to_string()], &emitter, 0);
        let events = crate::events::committed_journal_text(&home.events_jsonl());
        let expired = events
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .find(|v| v.get("type").and_then(Value::as_str) == Some("merge_cleanup_expired"))
            .expect("the expiry was emitted");
        assert_eq!(expired["data"]["last_hold"], "none", "{expired}");
        std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
    }

    #[test]
    fn a_late_mint_with_an_old_merge_is_not_expired_at_first_sight() {
        let home = temp_home("late-mint");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let now = chrono::Utc::now().to_rfc3339();
        write_events(
            &home,
            &[request_line(
                &now,
                "merge-cleanup-late",
                Some("2026-09-12T00:00:00Z"),
                json!(["x-1"]),
            )],
        );
        std::fs::create_dir_all(home.root()).unwrap();
        std::fs::write(home.root().join("merge-reap.stamp"), "0").unwrap();

        consume_merge_cleanup_requests(&home, &["/repo".to_string()], &emitter, 0);

        let events = crate::events::committed_journal_text(&home.events_jsonl());
        assert!(
            !events.contains("merge_cleanup_expired"),
            "a fresh mint must get its full payment window: {events}"
        );
        std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
    }

    /// A registry with one claude candidate row (plus, optionally, one
    /// crowned row the reaper must name and keep).
    fn write_registry(home: &AgentsHome, entries: &[Value]) {
        std::fs::create_dir_all(home.root()).unwrap();
        let registry = json!({"schema_version": 10, "agents": entries});
        std::fs::write(
            home.registry_json(),
            serde_json::to_string(&registry).unwrap(),
        )
        .unwrap();
    }

    fn claude_row(name: &str, crowned: bool) -> Value {
        let mut row = json!({
            "name": name,
            "cwd": "/repo/wt",
            "status": "exited",
            "created_at": "2026-09-06T00:00:00Z",
            "harness": "claude",
            "harness_session_id": format!("sess-{name}"),
            "short_id": "abc123",
            "origin": "spawn",
        });
        if crowned {
            row["crown_level"] = json!(1);
        }
        row
    }

    fn merged_states() -> Option<HashMap<String, (String, Option<String>, usize)>> {
        let mut states = HashMap::new();
        states.insert(
            "x-1".to_string(),
            ("done".to_string(), Some("merged".to_string()), 0),
        );
        Some(states)
    }

    fn settled_request(worktree: &str) -> MergeCleanupRequest {
        MergeCleanupRequest {
            request_id: "merge-cleanup-1".to_string(),
            repo: "/repo".to_string(),
            pr: 42,
            branch: Some("feature/x".to_string()),
            worktree: Some(worktree.to_string()),
            node_ids: vec!["x-1".to_string()],
            candidate_row_names: Vec::new(),
            merged_at: None,
            ts_unix: 0,
            session_id: None,
            harness: None,
        }
    }

    #[test]
    fn order_is_stop_then_rm_then_tree_then_completed() {
        // AC2-ORDER, with the tree subprocess behind a recording seam and the
        // row removal now real (the shared commit writes this fixture's own
        // registry): the events must read, in order, stop -> agent_row_reaped
        // -> worktree_removed -> merge_cleanup_completed, and the CALLS must
        // interleave stop, the native surface removal, take_tree in that same
        // order. A row dropped before its stop (or a tree event before the row
        // events) is the orphan-the-session bug this order exists to prevent.
        let home = temp_home("order");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        write_registry(&home, &[claude_row("target-x-1-worker", false)]);
        let wt = home.root().parent().unwrap().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let request = settled_request(wt.to_str().unwrap());
        let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));

        let stop_calls = std::rc::Rc::clone(&calls);
        let surface_calls = std::rc::Rc::clone(&calls);
        let tree_calls = std::rc::Rc::clone(&calls);
        let seams = RequestSeams {
            finished: &|_entry| true,
            stop: &|_entry| {
                stop_calls.borrow_mut().push("stop".to_string());
                Ok("abc123".to_string())
            },
            surface_removal: &|entry| {
                surface_calls
                    .borrow_mut()
                    .push(format!("surface:{}", entry.name));
                crate::daemon::CascadeOutcome::Removed
            },
            mux_member: &|_entry| crate::daemon::CascadeOutcome::NotApplicable,
            take_tree: &|_wt, _root| {
                tree_calls.borrow_mut().push("take_tree".to_string());
                true
            },
            tree_busy: &|_wt| Ok(false),
        };
        let (acted, held) = run_request(
            &home,
            &emitter,
            &request,
            "/repo",
            merged_states().as_ref(),
            None,
            1_000_000,
            &seams,
            None,
        );
        assert_eq!(acted, 2, "one row + one tree");
        assert!(!held, "the settled request completed");

        let kinds: Vec<String> = crate::events::committed_journal_text(&home.events_jsonl())
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(&l).ok())
            .filter_map(|v| v.get("type").and_then(Value::as_str).map(str::to_string))
            .collect();
        let stopped = kinds
            .iter()
            .position(|k| k == "merge_reaper_stopped")
            .unwrap();
        let reaped = kinds.iter().position(|k| k == "agent_row_reaped").unwrap();
        let removed = kinds.iter().position(|k| k == "worktree_removed").unwrap();
        let completed = kinds
            .iter()
            .position(|k| k == "merge_cleanup_completed")
            .unwrap();
        assert!(
            stopped < reaped && reaped < removed,
            "stop, then the row, then the tree: {kinds:?}"
        );
        assert!(
            removed < completed,
            "tree must precede the receipt: {kinds:?}"
        );
        let calls = calls.borrow();
        assert_eq!(
            *calls,
            vec![
                "stop".to_string(),
                "surface:target-x-1-worker".to_string(),
                "take_tree".to_string()
            ],
            "stop, then the native surface removal, then the tree: {calls:?}"
        );
        std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
    }

    #[test]
    fn crowned_row_is_named_and_kept() {
        // AC2-CROWN: an idle king reads state=done; exclusion is by name.
        let home = temp_home("crown");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        write_registry(
            &home,
            &[
                claude_row("target-x-1-worker", false),
                claude_row("king-x-1", true),
            ],
        );
        let request = settled_request("/repo/wt");
        let seams = RequestSeams {
            finished: &|_entry| true,
            stop: &|_entry| Ok("abc123".to_string()),
            surface_removal: &|_entry| crate::daemon::CascadeOutcome::Removed,
            mux_member: &|_entry| crate::daemon::CascadeOutcome::NotApplicable,
            take_tree: &|_wt, _root| true,
            tree_busy: &|_wt| Ok(false),
        };
        run_request(
            &home,
            &emitter,
            &request,
            "/repo",
            merged_states().as_ref(),
            None,
            1_000_000,
            &seams,
            None,
        );
        let events = crate::events::committed_journal_text(&home.events_jsonl());
        let completed = events
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(&l).ok())
            .find(|v| v.get("type").and_then(Value::as_str) == Some("merge_cleanup_completed"))
            .unwrap();
        let kept: Vec<String> = completed["data"]["kept"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(
            kept.iter()
                .any(|k| k.as_str().starts_with("king-x-1:kept_crowned")),
            "the crowned row must be named under kept_crowned: {kept:?}"
        );
        std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
    }

    #[test]
    fn open_node_holds_and_names_itself() {
        // AC2-EDGE: a node that is not done+merged holds the request, names
        // the reason, and removes nothing.
        let home = temp_home("held");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        write_registry(&home, &[claude_row("target-x-1-worker", false)]);
        let mut states = HashMap::new();
        states.insert("x-1".to_string(), ("in_progress".to_string(), None, 0));
        let request = settled_request("/repo/wt");
        let seams = RequestSeams {
            finished: &|_entry| true,
            stop: &|_entry| Ok("abc123".to_string()),
            surface_removal: &|_entry| crate::daemon::CascadeOutcome::Removed,
            mux_member: &|_entry| crate::daemon::CascadeOutcome::NotApplicable,
            take_tree: &|_wt, _root| true,
            tree_busy: &|_wt| Ok(false),
        };
        let (acted, held) = run_request(
            &home,
            &emitter,
            &request,
            "/repo",
            Some(&states),
            None,
            1_000_000,
            &seams,
            None,
        );
        assert_eq!(acted, 0);
        assert!(held, "an open node holds the request");
        let events = crate::events::committed_journal_text(&home.events_jsonl());
        assert!(
            events.contains("\"type\":\"merge_cleanup_held\"") && events.contains("node-open:x-1"),
            "the hold must name the open node: {events}"
        );
        assert!(
            !events.contains("merge_cleanup_completed"),
            "a held request must not emit a receipt: {events}"
        );
        std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
    }

    #[test]
    fn a_done_merged_tree_ignores_unpushed_head() {
        // A done and merged node's tree is removable when no process owns its
        // cwd, regardless of whether its local branch reaches origin/main.
        let home = temp_home("tree-done-merge");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        write_registry(&home, &[claude_row("target-x-1-worker", false)]);
        let wt = home.root().parent().unwrap().join("wt2");
        std::fs::create_dir_all(&wt).unwrap();
        let request = settled_request(wt.to_str().unwrap());
        let seams = RequestSeams {
            finished: &|_entry| true,
            stop: &|_entry| Ok("abc123".to_string()),
            surface_removal: &|_entry| crate::daemon::CascadeOutcome::Removed,
            mux_member: &|_entry| crate::daemon::CascadeOutcome::NotApplicable,
            take_tree: &|_wt, _root| true,
            tree_busy: &|_wt| Ok(false),
        };
        let (acted, held) = run_request(
            &home,
            &emitter,
            &request,
            "/repo",
            merged_states().as_ref(),
            None,
            1_000_000,
            &seams,
            None,
        );
        assert_eq!(acted, 2, "the row and squash-merged tree are removed");
        assert!(
            !held,
            "an unpushed local HEAD is not a hold for a done merge"
        );
        let events = crate::events::committed_journal_text(&home.events_jsonl());
        assert!(
            events.contains("\"type\":\"merge_cleanup_completed\"")
                && events.contains("\"tree\":\"removed\""),
            "the done merge must settle and remove the tree: {events}"
        );
        std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
    }

    #[test]
    fn a_process_cwd_keeps_the_tree_pending() {
        let home = temp_home("process-cwd");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        write_registry(&home, &[claude_row("target-x-1-worker", false)]);
        let wt = home.root().parent().unwrap().join("wt-process");
        std::fs::create_dir_all(&wt).unwrap();
        let request = settled_request(wt.to_str().unwrap());
        let seams = RequestSeams {
            finished: &|_entry| true,
            stop: &|_entry| Ok("abc123".to_string()),
            surface_removal: &|_entry| crate::daemon::CascadeOutcome::Removed,
            mux_member: &|_entry| crate::daemon::CascadeOutcome::NotApplicable,
            tree_busy: &|_wt| Ok(true),
            take_tree: &|_wt, _root| panic!("a busy tree must not be removed"),
        };

        let (acted, held) = run_request(
            &home,
            &emitter,
            &request,
            "/repo",
            merged_states().as_ref(),
            None,
            1_000_000,
            &seams,
            None,
        );

        assert_eq!(acted, 1, "the row is removed, the busy tree is not");
        assert!(held);
        let events = crate::events::committed_journal_text(&home.events_jsonl());
        assert!(events.contains("tree-held:process-cwd"), "events: {events}");
        assert!(!events.contains("worktree_removed"), "events: {events}");
        std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
    }

    #[test]
    fn an_unreadable_process_cwd_probe_keeps_the_tree_pending() {
        let home = temp_home("process-cwd-unreadable");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        write_registry(&home, &[claude_row("target-x-1-worker", false)]);
        let wt = home.root().parent().unwrap().join("wt-process");
        std::fs::create_dir_all(&wt).unwrap();
        let request = settled_request(wt.to_str().unwrap());
        let seams = RequestSeams {
            finished: &|_entry| true,
            stop: &|_entry| Ok("abc123".to_string()),
            surface_removal: &|_entry| crate::daemon::CascadeOutcome::Removed,
            mux_member: &|_entry| crate::daemon::CascadeOutcome::NotApplicable,
            tree_busy: &|_wt| Err(()),
            take_tree: &|_wt, _root| panic!("an unreadable probe must not remove a tree"),
        };

        let (acted, held) = run_request(
            &home,
            &emitter,
            &request,
            "/repo",
            merged_states().as_ref(),
            None,
            1_000_000,
            &seams,
            None,
        );

        assert_eq!(acted, 1);
        assert!(held);
        let events = crate::events::committed_journal_text(&home.events_jsonl());
        assert!(
            events.contains("tree-held:cwd-unreadable"),
            "events: {events}"
        );
        std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
    }

    #[test]
    fn a_merge_retirement_leaves_a_resumable_receipt() {
        // The delegation's payoff: the merge path no longer shells out to
        // `fno agents rm`, so a merge-triggered removal stages the SAME
        // receipt the scheduled sweep does - the resume form plus the typed
        // native effect. Before this, a merged worker's row vanished with no
        // effect record naming what the native removal actually answered.
        let home = temp_home("receipt");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        write_registry(&home, &[claude_row("target-x-1-worker", false)]);
        let request = settled_request("/repo/wt");
        let seams = RequestSeams {
            finished: &|_entry| true,
            stop: &|_entry| Ok("abc123".to_string()),
            surface_removal: &|_entry| crate::daemon::CascadeOutcome::Removed,
            mux_member: &|_entry| crate::daemon::CascadeOutcome::NotApplicable,
            take_tree: &|_wt, _root| true,
            tree_busy: &|_wt| Ok(false),
        };
        run_request(
            &home,
            &emitter,
            &request,
            "/repo",
            merged_states().as_ref(),
            None,
            1_000_000,
            &seams,
            None,
        );
        let dir = home.root().join("reap-receipts");
        let files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        assert_eq!(files.len(), 1, "one row retired, one receipt: {files:?}");
        let receipt = crate::receipt::read_reap_receipt(&files[0]).unwrap();
        assert_eq!(receipt.row_name, "target-x-1-worker");
        assert!(
            !receipt.resume_argv.is_empty(),
            "the receipt must carry the resume form: {receipt:?}"
        );
        let effects: std::collections::BTreeMap<String, String> = receipt
            .effects
            .iter()
            .map(|effect| (effect.op.clone(), effect.outcome.clone()))
            .collect();
        assert_eq!(
            effects.get("native-stop").map(String::as_str),
            Some("confirmed-removed"),
            "the confirmed stop must be named: {receipt:?}"
        );
        assert_eq!(
            effects.get("active-surface").map(String::as_str),
            Some("confirmed-removed"),
            "the native active-surface outcome must be named: {receipt:?}"
        );
        assert!(
            effects.contains_key("resume-evidence"),
            "the resumability evidence op must be present: {receipt:?}"
        );
        std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
    }

    #[test]
    fn an_unconfirmed_native_removal_keeps_the_row() {
        // The applied gate, inherited from the sweep: a `kept` (unverified)
        // native outcome holds the row for the next pass instead of dropping
        // it. The old rm subprocess had no such reading - it either exited 0
        // or the whole request refused.
        let home = temp_home("unconfirmed");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        write_registry(&home, &[claude_row("target-x-1-worker", false)]);
        let request = settled_request("/repo/wt");
        let seams = RequestSeams {
            finished: &|_entry| true,
            stop: &|_entry| Ok("abc123".to_string()),
            surface_removal: &|_entry| {
                crate::daemon::CascadeOutcome::Unverified("roster unreadable".into())
            },
            mux_member: &|_entry| crate::daemon::CascadeOutcome::NotApplicable,
            take_tree: &|_wt, _root| true,
            tree_busy: &|_wt| Ok(false),
        };
        let (acted, held) = run_request(
            &home,
            &emitter,
            &request,
            "/repo",
            merged_states().as_ref(),
            None,
            1_000_000,
            &seams,
            None,
        );
        assert_eq!(acted, 0, "no row removed on an unconfirmed native removal");
        assert!(held, "an unverified effect holds the request");
        let registry = state::load_registry(&home.registry_json()).unwrap();
        assert_eq!(registry.entries.len(), 1, "the row is kept for retry");
        let events = crate::events::committed_journal_text(&home.events_jsonl());
        assert!(
            events.contains("target-x-1-worker:native_removal_unconfirmed"),
            "the kept row must name the effect that held it, not the stop: {events}"
        );
        assert!(held, "an unverified effect holds the request");
        std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
    }

    #[test]
    fn merge_trigger_matches_every_worker_prefix() {
        // The operator's naming convention mints t-, bp-, king- and target-
        // rows; the join must read all four from the node hex. The cwd leg
        // stays out of the way: no row's cwd matches the request worktree.
        let home = temp_home("prefixes");
        write_registry(
            &home,
            &[
                claude_row("t-aaaa-idle-glm", false),
                claude_row("bp-aaaa-arm-timeout", false),
                claude_row("king-aaaa", false),
                claude_row("target-x-aaaa-worker", false),
                // Another node's worker: never a candidate.
                claude_row("t-docs-3prs-glm", false),
            ],
        );
        let mut request = settled_request("/elsewhere");
        request.node_ids = vec!["x-aaaa".to_string()];
        let rows = merge_cleanup_rows(&home, &request);
        let names: Vec<String> = rows.iter().map(|e| e.name.clone()).collect();
        for expected in [
            "t-aaaa-idle-glm",
            "bp-aaaa-arm-timeout",
            "king-aaaa",
            "target-x-aaaa-worker",
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "the merge trigger must match {expected}: {names:?}"
            );
        }
        assert!(
            !names.iter().any(|n| n == "t-docs-3prs-glm"),
            "another node's row is not a candidate: {names:?}"
        );
        std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
    }

    #[test]
    fn merge_pass_drops_the_finished_row_and_only_that_row() {
        // The outcome test the node demands: the registry delta itself, with
        // the untouched row as the positive control in the SAME read.
        let home = temp_home("outcome");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let mut busy = claude_row("t-ffff-busy-glm", false);
        busy["short_id"] = json!("fff001");
        write_registry(&home, &[claude_row("t-aaaa-idle-glm", false), busy]);
        let mut states = HashMap::new();
        states.insert(
            "x-aaaa".to_string(),
            ("done".to_string(), Some("merged".to_string()), 0),
        );
        states.insert("x-ffff".to_string(), ("in_progress".to_string(), None, 0));
        let mut request = settled_request("/no-such-worktree");
        request.node_ids = vec!["x-aaaa".to_string()];
        let seams = RequestSeams {
            finished: &|_entry| true,
            stop: &|_entry| Ok("abc123".to_string()),
            surface_removal: &|_entry| crate::daemon::CascadeOutcome::Removed,
            mux_member: &|_entry| crate::daemon::CascadeOutcome::NotApplicable,
            take_tree: &|_wt, _root| true,
            tree_busy: &|_wt| Ok(false),
        };
        let before = state::load_registry(&home.registry_json())
            .unwrap()
            .entries
            .len();
        let (acted, held) = run_request(
            &home,
            &emitter,
            &request,
            "/repo",
            Some(&states),
            None,
            1_000_000,
            &seams,
            None,
        );
        assert_eq!(acted, 1, "one row removed");
        assert!(!held, "the request completed");
        let after = state::load_registry(&home.registry_json()).unwrap();
        assert_eq!(
            after.entries.len(),
            before - 1,
            "the count dropped by exactly one"
        );
        assert!(
            !after.entries.iter().any(|e| e.name == "t-aaaa-idle-glm"),
            "the finished row is gone BY NAME"
        );
        assert!(
            after.entries.iter().any(|e| e.name == "t-ffff-busy-glm"),
            "the positive control is still present BY NAME"
        );
        std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
    }

    #[test]
    fn one_request_holding_does_not_block_another() {
        // The killed hypothesis, as a test: a request holding on an open
        // node never gates a sibling request's removal in the same pass.
        let home = temp_home("independence");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let mut open_row = claude_row("t-cccc-open-glm", false);
        open_row["short_id"] = json!("cccc01");
        write_registry(&home, &[open_row, claude_row("t-aaaa-idle-glm", false)]);
        let mut states = HashMap::new();
        states.insert(
            "x-aaaa".to_string(),
            ("done".to_string(), Some("merged".to_string()), 0),
        );
        states.insert("x-cccc".to_string(), ("in_progress".to_string(), None, 0));
        let mut holding = settled_request("/no-such-worktree");
        holding.node_ids = vec!["x-cccc".to_string()];
        let mut clean = settled_request("/no-such-worktree");
        clean.node_ids = vec!["x-aaaa".to_string()];
        let seams = RequestSeams {
            finished: &|_entry| true,
            stop: &|_entry| Ok("abc123".to_string()),
            surface_removal: &|_entry| crate::daemon::CascadeOutcome::Removed,
            mux_member: &|_entry| crate::daemon::CascadeOutcome::NotApplicable,
            take_tree: &|_wt, _root| true,
            tree_busy: &|_wt| Ok(false),
        };
        let (acted_a, held_a) = run_request(
            &home,
            &emitter,
            &holding,
            "/repo",
            Some(&states),
            None,
            1_000_000,
            &seams,
            None,
        );
        assert_eq!(acted_a, 0, "the holding request removes nothing");
        assert!(held_a, "the open node holds its own request");
        let (acted_b, held_b) = run_request(
            &home,
            &emitter,
            &clean,
            "/repo",
            Some(&states),
            None,
            1_000_001,
            &seams,
            None,
        );
        assert_eq!(
            acted_b, 1,
            "the clean request removes its row in the same pass"
        );
        assert!(!held_b, "the clean request completes");
        let after = state::load_registry(&home.registry_json()).unwrap();
        assert!(
            after.entries.iter().any(|e| e.name == "t-cccc-open-glm"),
            "the holding request's row is untouched"
        );
        assert!(
            !after.entries.iter().any(|e| e.name == "t-aaaa-idle-glm"),
            "the clean request's row is gone"
        );
        std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
    }

    #[test]
    fn a_writing_worker_survives_the_merge_pass() {
        // Change 2's arm: a worker that wrote moments ago with no terminal
        // roster state stays, the stop is never attempted, and the request
        // holds instead of completing.
        let home = temp_home("writing");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        write_registry(&home, &[claude_row("t-1-writing-glm", false)]);
        let states = merged_states();
        let request = settled_request("/no-such-worktree");
        let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        let stop_calls = std::rc::Rc::clone(&calls);
        let seams = RequestSeams {
            finished: &|_entry| false,
            stop: &|_entry| {
                stop_calls.borrow_mut().push("stop".to_string());
                Ok("abc123".to_string())
            },
            surface_removal: &|_entry| crate::daemon::CascadeOutcome::Removed,
            mux_member: &|_entry| crate::daemon::CascadeOutcome::NotApplicable,
            take_tree: &|_wt, _root| true,
            tree_busy: &|_wt| Ok(false),
        };
        let (acted, held) = run_request(
            &home,
            &emitter,
            &request,
            "/repo",
            states.as_ref(),
            None,
            1_000_000,
            &seams,
            None,
        );
        assert_eq!(acted, 0, "nothing removed");
        assert!(held, "the request holds for the writing worker");
        let after = state::load_registry(&home.registry_json()).unwrap();
        assert!(
            after.entries.iter().any(|e| e.name == "t-1-writing-glm"),
            "the writing worker's row stays"
        );
        let events = crate::events::committed_journal_text(&home.events_jsonl());
        assert!(
            !events.contains("merge_cleanup_completed"),
            "no completion tombstones a held request: {events}"
        );
        assert!(
            events.contains("still_writing"),
            "the hold names the kept row: {events}"
        );
        assert!(
            calls.borrow().is_empty(),
            "the stop is never attempted on a writing worker"
        );
        std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
    }

    #[test]
    fn an_already_gone_row_completes_with_none_present() {
        // Change 3's honest arm: a request whose join finds no row completes
        // with the positive marker, not an ambiguous empty list.
        let home = temp_home("none-present");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        write_registry(&home, &[]);
        let request = settled_request("/no-such-worktree");
        let seams = RequestSeams {
            finished: &|_entry| true,
            stop: &|_entry| Ok("abc123".to_string()),
            surface_removal: &|_entry| crate::daemon::CascadeOutcome::Removed,
            mux_member: &|_entry| crate::daemon::CascadeOutcome::NotApplicable,
            take_tree: &|_wt, _root| true,
            tree_busy: &|_wt| Ok(false),
        };
        let (acted, held) = run_request(
            &home,
            &emitter,
            &request,
            "/repo",
            merged_states().as_ref(),
            None,
            1_000_000,
            &seams,
            None,
        );
        assert_eq!(acted, 0);
        assert!(!held);
        let events = crate::events::committed_journal_text(&home.events_jsonl());
        assert!(
            events.contains("\"rows\":\"none-present\""),
            "the completion names the zero honestly: {events}"
        );
        std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
    }

    #[test]
    fn exact_candidates_select_the_registered_row() {
        // AC4-HP: a request carrying candidate_row_names retires the
        // EXACT registered row - ab-bp- spelling included - via membership,
        // never prefix reconstruction.
        let home = temp_home("exact-candidates");
        write_registry(&home, &[claude_row("ab-bp-x-1-cargo", false)]);
        let mut request = settled_request("/repo/other-wt");
        request.candidate_row_names = vec!["ab-bp-x-1-cargo".to_string()];
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let seams = RequestSeams {
            finished: &|_entry| true,
            stop: &|_entry| Ok("abc123".to_string()),
            surface_removal: &|_entry| crate::daemon::CascadeOutcome::Removed,
            mux_member: &|_entry| crate::daemon::CascadeOutcome::NotApplicable,
            take_tree: &|_wt, _root| true,
            tree_busy: &|_wt| Ok(false),
        };
        let (acted, _held) = run_request(
            &home,
            &emitter,
            &request,
            "/repo",
            merged_states().as_ref(),
            None,
            1_000_000,
            &seams,
            None,
        );
        assert_eq!(acted, 1, "the exact candidate row is selected: {acted}");
        std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
    }

    #[test]
    fn a_row_absent_from_candidates_is_never_removed() {
        // AC4-EDGE: the producer's candidate list is exact. A second
        // row of the same node, unproposed, stays; the name leg does not
        // fire either, because x-2 is not one of the closed nodes.
        let home = temp_home("absent-candidate");
        write_registry(&home, &[claude_row("ab-bp-x-2-research", false)]);
        let mut request = settled_request("/repo/other-wt");
        request.candidate_row_names = vec!["ab-bp-x-2-cargo".to_string()];
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let seams = RequestSeams {
            finished: &|_entry| true,
            stop: &|_entry| Ok("abc123".to_string()),
            surface_removal: &|_entry| crate::daemon::CascadeOutcome::Removed,
            mux_member: &|_entry| crate::daemon::CascadeOutcome::NotApplicable,
            take_tree: &|_wt, _root| true,
            tree_busy: &|_wt| Ok(false),
        };
        let (acted, _held) = run_request(
            &home,
            &emitter,
            &request,
            "/repo",
            merged_states().as_ref(),
            None,
            1_000_000,
            &seams,
            None,
        );
        assert_eq!(acted, 0, "an absent row is never removed: {acted}");
        std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
    }

    #[test]
    fn legacy_events_without_candidates_still_select_by_name_route() {
        // An event minted before the candidate field exists: the name leg
        // still selects the row - the shared name_route vocabulary resolves
        // the legacy target-<node>- spelling to the closed node.
        let home = temp_home("legacy-candidates");
        write_registry(&home, &[claude_row("target-x-1-worker", false)]);
        let mut request = settled_request("/repo/other-wt");
        request.worktree = None; // force the name path only
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let seams = RequestSeams {
            finished: &|_entry| true,
            stop: &|_entry| Ok("abc123".to_string()),
            surface_removal: &|_entry| crate::daemon::CascadeOutcome::Removed,
            mux_member: &|_entry| crate::daemon::CascadeOutcome::NotApplicable,
            take_tree: &|_wt, _root| true,
            tree_busy: &|_wt| Ok(false),
        };
        let (acted, _held) = run_request(
            &home,
            &emitter,
            &request,
            "/repo",
            merged_states().as_ref(),
            None,
            1_000_000,
            &seams,
            None,
        );
        assert_eq!(acted, 1, "the legacy fallback still selects: {acted}");
        std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
    }

    #[test]
    fn wrapped_row_joins_the_merge_cleanup_by_name() {
        // the widened name vocabulary joins wrapper-prefixed rows
        // too - a king-spawned row for a closed node is reaped, not left.
        let home = temp_home("wrapped-cleanup");
        write_registry(&home, &[claude_row("k-bp-x-1-cargo", false)]);
        let mut request = settled_request("/repo/other-wt");
        request.candidate_row_names = Vec::new(); // force the name path only
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let seams = RequestSeams {
            finished: &|_entry| true,
            stop: &|_entry| Ok("abc123".to_string()),
            surface_removal: &|_entry| crate::daemon::CascadeOutcome::Removed,
            mux_member: &|_entry| crate::daemon::CascadeOutcome::NotApplicable,
            take_tree: &|_wt, _root| true,
            tree_busy: &|_wt| Ok(false),
        };
        let (acted, _held) = run_request(
            &home,
            &emitter,
            &request,
            "/repo",
            merged_states().as_ref(),
            None,
            1_000_000,
            &seams,
            None,
        );
        assert_eq!(acted, 1, "the wrapped row still selects: {acted}");
        std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
    }
}
