//! The merge reaper (x-07dc): a PR merged, its node done, the machine reaps.
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

/// A request older than this past its merge moment expires instead of pinning
/// forever. Covers the broken-reconcile shape: the doneness re-read would
/// hold it every pass, and an unexpired hold is the one way this reaper can
/// grow the dead population it exists to bound.
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
    /// Unix seconds: when the merge landed (the grace anchor). A request
    /// without one (an older ritual mint) falls back to the envelope `ts`.
    merged_at: Option<i64>,
    ts_unix: i64,
    session_id: Option<String>,
    harness: Option<String>,
}

/// The pending set for one repo: every request minus the ones a tombstone
/// already settled. `merge_cleanup_completed` / `_refused` / `_expired` all
/// finish a request; a held request stays pending and is re-read every pass.
/// One journal read per call; the reaper pass calls the `_all` variant once
/// and partitions in memory, so N roots cost one read, not N.
pub(crate) fn pending_merge_cleanup_requests(
    home: &AgentsHome,
    repo: &str,
) -> Vec<MergeCleanupRequest> {
    pending_merge_cleanup_requests_all(home)
        .into_iter()
        .filter(|request| request.repo == repo)
        .collect()
}

/// Every pending request across repos, in one journal read.
fn pending_merge_cleanup_requests_all(home: &AgentsHome) -> Vec<MergeCleanupRequest> {
    let Ok(contents) = std::fs::read_to_string(home.events_jsonl()) else {
        return Vec::new();
    };
    let mut requested = BTreeMap::<String, MergeCleanupRequest>::new();
    let mut finished = HashSet::<String>::new();
    for line in contents.lines() {
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
                requested.insert(
                    request_id.to_owned(),
                    MergeCleanupRequest {
                        request_id: request_id.to_owned(),
                        repo: request_repo.to_owned(),
                        pr,
                        branch: string_field("branch"),
                        worktree: string_field("worktree"),
                        node_ids: strings("node_ids"),
                        merged_at,
                        ts_unix,
                        session_id: string_field("session_id"),
                        harness: string_field("harness"),
                    },
                );
            }
            "merge_cleanup_completed" | "merge_cleanup_refused" | "merge_cleanup_expired" => {
                finished.insert(request_id.to_owned());
            }
            _ => {}
        }
    }
    requested
        .into_values()
        .filter(|request| !finished.contains(&request.request_id))
        .collect()
}

/// True while any pending request stands for the repo: the worktree sweep's
/// apply gate (a standing request means the pass may apply, not just report).
pub(crate) fn merge_cleanup_requested(home: &AgentsHome, repo: &str) -> bool {
    pending_merge_cleanup_requests_all(home)
        .iter()
        .any(|request| request.repo == repo)
}

/// The rows this request may remove: registry rows whose cwd IS the merged
/// worktree or whose name was minted for one of the closed nodes. Sorted by
/// name. A request-named row the registry no longer carries is ALREADY gone,
/// so it is not a candidate: a re-pass after a held tree must read the row set
/// as settled, not wedged.
fn merge_cleanup_rows(
    home: &AgentsHome,
    request: &MergeCleanupRequest,
) -> Vec<state::RegistryEntry> {
    let Ok(registry) = state::load_registry(&home.registry_json()) else {
        return Vec::new();
    };
    let mut rows: Vec<state::RegistryEntry> = registry
        .entries
        .into_iter()
        .filter(|entry| {
            request
                .worktree
                .as_deref()
                .is_some_and(|worktree| entry.cwd == worktree)
                || request
                    .node_ids
                    .iter()
                    .any(|node| entry.name.starts_with(&format!("target-{node}-")))
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows.dedup_by(|a, b| a.name == b.name);
    rows
}

/// Stop the row's harness before the registry row drops. `Some(short)` is the
/// confirmed stop (empty for a pane row, whose pane `fno agents rm` already
/// kills); `None` refuses and keeps the row for the next pass.
fn stop_harness_confirmed(
    home: &AgentsHome,
    entry: &state::RegistryEntry,
) -> Result<String, &'static str> {
    // A mux row's pane death IS the harness stop; rm handles it.
    if entry.mux.is_some() {
        return Ok(String::new());
    }
    // Positive death evidence first: a finished claude agent never leaves the
    // roster, so its stop can never be confirmed by absence. One snapshot
    // read per merge cleanup - the request names a handful of rows at most.
    if crate::gc_sweep::claude_death_reason(entry, &crate::claude_roster::read_all_agents())
        .is_some()
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

/// The ONE remaining tree guard (the dirty guard is gone for a done node, by
/// the operator's 2026-09-07 ruling): a HEAD that is not an ancestor of
/// origin/main is unpushed work, and unpushed work holds. An unreadable
/// origin holds for the same reason - a removal never guesses.
fn tree_unreachable_from_origin_main(worktree: &str) -> bool {
    !std::process::Command::new("git")
        .current_dir(worktree)
        .args(["merge-base", "--is-ancestor", "HEAD", "origin/main"])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Forced removal from inside the leaf (git allows it), then a prune in the
/// canonical checkout. The branch is never deleted: `git worktree remove`
/// does not touch refs, and the branch is the recovery path.
fn remove_tree(worktree: &str, repo_root: &str) -> bool {
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
    let last_echo = std::fs::read_to_string(&stamp)
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0);
    if now.saturating_sub(last_echo) < MERGE_REAP_HOLD_ECHO_SECS {
        return;
    }
    let _ = std::fs::write(&stamp, now.to_string());
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
    /// Stop the row's harness. `Ok(short)` names the stopped session (empty
    /// for a pane row); `Err` holds the row for this pass.
    stop: &'a dyn Fn(&state::RegistryEntry) -> Result<String, &'static str>,
    /// The native ACTIVE-SURFACE removal, typed: claude's agent list, codex's
    /// session index, cursor-agent's worker servers.
    surface_removal: &'a dyn Fn(&state::RegistryEntry) -> crate::daemon::CascadeOutcome,
    /// The ONE tree guard: true = unpushed work, hold.
    tree_holds: &'a dyn Fn(&str) -> bool,
    /// Forced tree removal; true = gone (the caller emits and prunes).
    take_tree: &'a dyn Fn(&str, &str) -> bool,
}

/// Settle one request past its grace window. Returns the acted count (rows
/// removed + trees removed). Every step names itself in events.jsonl IN
/// ORDER: merge_reaper_stopped, agent_row_reaped (emitted by the shared
/// commit), worktree_removed, merge_cleanup_completed.
fn run_request(
    home: &AgentsHome,
    emitter: &EventEmitter,
    request: &MergeCleanupRequest,
    root: &str,
    states: Option<&HashMap<String, (String, Option<String>)>>,
    ledger: Option<&[Value]>,
    now: i64,
    seams: &RequestSeams,
) -> u64 {
    // 2. Doneness re-read: every named node must read done AND merged. An
    // open node holds (a merge whose reconcile failed must not reap a worker
    // still working an open node); an EMPTY node list holds too - no proof,
    // no removal. A graph that will not read holds for the same reason.
    let Some(states) = states else {
        emit_hold_once_per_hour(home, emitter, request, "graph-unreadable", now);
        return 0;
    };
    let open: Vec<String> = request
        .node_ids
        .iter()
        .filter(|id| {
            states
                .get(id.as_str())
                .map(|(status, merge_status)| {
                    status != "done" || merge_status.as_deref() != Some("merged")
                })
                .unwrap_or(true)
        })
        .cloned()
        .collect();
    if request.node_ids.is_empty() || !open.is_empty() {
        let reason = match open.first() {
            Some(id) => format!("node-open:{id}"),
            None => "no-node-ids".to_string(),
        };
        emit_hold_once_per_hour(home, emitter, request, &reason, now);
        return 0;
    }
    // 3. Candidates, with the crowned and operator-origin rows named out: an
    // idle king reads state=done, so exclusion is by NAME, never by roster
    // state.
    let mut kept: Vec<String> = Vec::new();
    let mut rows: Vec<state::RegistryEntry> = Vec::new();
    for entry in merge_cleanup_rows(home, request) {
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
        match crate::gc_sweep::stage_session_retirement(
            home,
            entry,
            ledger,
            false,
            &stop,
            seams.surface_removal,
            &mut receipts,
        ) {
            Ok(()) => {
                to_retire.insert(
                    entry.name.clone(),
                    crate::gc_sweep::RetireOrder {
                        id: entry.name.clone(),
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
            Err(refusal) => kept.push(format!(
                "{name}:{reason}",
                name = entry.name,
                reason = match refusal {
                    crate::gc_sweep::RetireRefusal::StopRefused(_) => "stop_refused",
                    crate::gc_sweep::RetireRefusal::NativeRemoval(_) =>
                        "native_removal_unconfirmed",
                    crate::gc_sweep::RetireRefusal::NoReceipt(_) => "no_receipt",
                    crate::gc_sweep::RetireRefusal::GraphObligation(_) => "open_do_row",
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
        return 0;
    }
    let removed_rows: Vec<String> = report.retired_names.iter().cloned().collect();
    // 6. The tree, after the rows: whatever its git status, a done and
    // merged node's tree goes; the branch and the transcript are the
    // recovery path. Unpushed (HEAD not in origin/main) holds. A HELD tree
    // keeps the request pending - the rows are already gone and idempotent
    // to re-read, so a later pass can take the tree once the hold clears
    // (the branch pushed), instead of tombstoning the hold forever.
    let mut reclaimed_bytes: u64 = 0;
    let mut tree_note = "no-worktree";
    if let Some(worktree) = request.worktree.as_deref() {
        if std::path::Path::new(worktree).exists() {
            if (seams.tree_holds)(worktree) {
                emit_hold_once_per_hour(
                    home,
                    emitter,
                    request,
                    "tree-held:unreachable-from-origin-main",
                    now,
                );
                return removed_rows.len() as u64;
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
                return removed_rows.len() as u64;
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
            "tree": tree_note,
        }),
    );
    let _ = std::fs::remove_file(hold_stamp_path(home, &request.request_id));
    removed_rows.len() as u64 + u64::from(tree_note == "removed")
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
    let pending = pending_merge_cleanup_requests_all(home);

    let mut total_requests = 0usize;
    let mut in_grace = 0usize;
    let mut acted: u64 = 0;
    for root in roots {
        for request in pending.iter().filter(|r| r.repo == *root) {
            total_requests += 1;
            let merged_at = request.merged_at.unwrap_or(request.ts_unix);
            let age = now.saturating_sub(merged_at);
            // 1. Grace: a clock from the merge moment, never a liveness
            // probe. A session stopped inside the window stays resumable
            // through its transcript, which is what makes the wait safe.
            if age < grace_secs.max(0) {
                in_grace += 1;
                continue;
            }
            if age > MERGE_REAP_EXPIRY_SECS {
                let _ = emitter.emit(
                    "merge_cleanup_expired",
                    &json!({
                        "request_id": request.request_id,
                        "repo": request.repo,
                        "pr": request.pr,
                        "reason": "expired",
                    }),
                );
                let _ = std::fs::remove_file(hold_stamp_path(home, &request.request_id));
                continue;
            }
            let seams = RequestSeams {
                stop: &|entry| stop_harness_confirmed(home, entry),
                surface_removal: &crate::gc_native::apply_active_surface_removal,
                tree_holds: &tree_unreachable_from_origin_main,
                take_tree: &remove_tree,
            };
            acted += run_request(
                home,
                emitter,
                request,
                root,
                node_states.as_ref(),
                ledger.as_deref(),
                now,
                &seams,
            );
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
        Some(&format!("requests={total_requests}")),
        MERGE_REAP_INTERVAL_SECS,
    );
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
        let events = std::fs::read_to_string(home.events_jsonl()).unwrap();
        assert!(
            events.contains("\"skip_reason\":\"all_in_grace\""),
            "the tick row must name the grace hold: {events}"
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

    fn merged_states() -> Option<HashMap<String, (String, Option<String>)>> {
        let mut states = HashMap::new();
        states.insert(
            "x-1".to_string(),
            ("done".to_string(), Some("merged".to_string())),
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
            tree_holds: &|_wt| false,
            take_tree: &|_wt, _root| {
                tree_calls.borrow_mut().push("take_tree".to_string());
                true
            },
        };
        let acted = run_request(
            &home,
            &emitter,
            &request,
            "/repo",
            merged_states().as_ref(),
            None,
            1_000_000,
            &seams,
        );
        assert_eq!(acted, 2, "one row + one tree");

        let kinds: Vec<String> = std::fs::read_to_string(home.events_jsonl())
            .unwrap()
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
            stop: &|_entry| Ok("abc123".to_string()),
            surface_removal: &|_entry| crate::daemon::CascadeOutcome::Removed,
            tree_holds: &|_wt| false,
            take_tree: &|_wt, _root| true,
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
        );
        let events = std::fs::read_to_string(home.events_jsonl()).unwrap();
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
        states.insert("x-1".to_string(), ("in_progress".to_string(), None));
        let request = settled_request("/repo/wt");
        let seams = RequestSeams {
            stop: &|_entry| Ok("abc123".to_string()),
            surface_removal: &|_entry| crate::daemon::CascadeOutcome::Removed,
            tree_holds: &|_wt| false,
            take_tree: &|_wt, _root| true,
        };
        let acted = run_request(
            &home,
            &emitter,
            &request,
            "/repo",
            Some(&states),
            None,
            1_000_000,
            &seams,
        );
        assert_eq!(acted, 0);
        let events = std::fs::read_to_string(home.events_jsonl()).unwrap();
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
    fn held_tree_keeps_the_request_pending() {
        // A tree that holds (unpushed HEAD) names the hold, removes its rows,
        // and does NOT settle the request: a later pass takes the tree once
        // the hold clears, instead of tombstoning the hold forever.
        let home = temp_home("tree-hold");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        write_registry(&home, &[claude_row("target-x-1-worker", false)]);
        let wt = home.root().parent().unwrap().join("wt2");
        std::fs::create_dir_all(&wt).unwrap();
        let request = settled_request(wt.to_str().unwrap());
        let seams = RequestSeams {
            stop: &|_entry| Ok("abc123".to_string()),
            surface_removal: &|_entry| crate::daemon::CascadeOutcome::Removed,
            tree_holds: &|_wt| true,
            take_tree: &|_wt, _root| true,
        };
        let acted = run_request(
            &home,
            &emitter,
            &request,
            "/repo",
            merged_states().as_ref(),
            None,
            1_000_000,
            &seams,
        );
        assert_eq!(acted, 1, "the row is removed, the tree is not");
        let events = std::fs::read_to_string(home.events_jsonl()).unwrap();
        assert!(
            events.contains("tree-held:unreachable-from-origin-main"),
            "the hold must name the tree: {events}"
        );
        assert!(
            !events.contains("merge_cleanup_completed"),
            "a tree-held request must not settle: {events}"
        );
        // Pass one really removed the row (the shared commit writes this
        // fixture's registry), so the second pass reads an empty candidate
        // set and takes no action.
        let second = run_request(
            &home,
            &emitter,
            &request,
            "/repo",
            merged_states().as_ref(),
            None,
            1_000_001,
            &seams,
        );
        assert_eq!(second, 0, "nothing left to remove");
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
            stop: &|_entry| Ok("abc123".to_string()),
            surface_removal: &|_entry| crate::daemon::CascadeOutcome::Removed,
            tree_holds: &|_wt| false,
            take_tree: &|_wt, _root| true,
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
            stop: &|_entry| Ok("abc123".to_string()),
            surface_removal: &|_entry| {
                crate::daemon::CascadeOutcome::Unverified("roster unreadable".into())
            },
            tree_holds: &|_wt| false,
            take_tree: &|_wt, _root| true,
        };
        let acted = run_request(
            &home,
            &emitter,
            &request,
            "/repo",
            merged_states().as_ref(),
            None,
            1_000_000,
            &seams,
        );
        assert_eq!(acted, 0, "no row removed on an unconfirmed native removal");
        let registry = state::load_registry(&home.registry_json()).unwrap();
        assert_eq!(registry.entries.len(), 1, "the row is kept for retry");
        let events = std::fs::read_to_string(home.events_jsonl()).unwrap();
        assert!(
            events.contains("target-x-1-worker:native_removal_unconfirmed"),
            "the kept row must name the effect that held it, not the stop: {events}"
        );
        std::fs::remove_dir_all(home.root().parent().unwrap()).ok();
    }
}
