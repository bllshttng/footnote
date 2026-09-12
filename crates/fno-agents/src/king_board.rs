//! The king board's budget-aware collector, in Rust (x-25b8).
//!
//! Ports `cli/src/fno/king/board.py`'s `build_board` + `collect_inputs` with
//! the budget branch built in (the x-f8e3 reference, `feature/x-f8e3`
//! bfd1a5e8e, which could not land because the file-budget gate refuses grown
//! Python): the caller hands in ONE whole-board budget, every per-source slice
//! derives from it, and as the budget runs out the board stops starting reads,
//! marks each unstarted source, and still emits the payload it has.
//!
//! The speed win is in-process reads. One graph read (the same
//! `read_defaulted_opts(path, false, false)` the keeper's `read_strict` runs)
//! feeds claimed-node lookups, PR binding, crown scope, AND the unplanned
//! queue's ready selection (the same `backlog_ready::select` the verb
//! serves). The claims merge is a directory scan. `needs` folds in-process
//! over the same sources `fno
//! agents needs` reads. Three source reads stay subprocesses: `gh pr list` (a
//! real network boundary), `fno inbox outstanding`
//! (measured 2026-09-04: 1.12s wall at load 52, far under its 10s bar - the
//! plan's change 2 keeps it and records the measurement), and `fno backlog
//! undispatched`, which used to classify the graph in-process here. That copy
//! ordered the board differently from the Python selection key, so the two
//! named different next nodes on one graph. One implementation costs one
//! spawn. The batched truth probe is a fifth spawn: one interpreter per holder
//! it measures, when any holder exists.
//!
//! Output keeps the Python JSON shape: `actionable`, `unreadable`, `queues`
//! (same names, same order, same row dicts), `warnings`, `exit_code` - plus a
//! per-source `sources` map carrying `ok`/`truncated` that the plan's
//! done-probe reads. `parse_king_board_value` in loopcheck.rs ignores the
//! extra key, so every existing reader is unchanged.
//!
//! Module layout: `budget` (the one whole-board budget + bounded
//! subprocess runner), `claims` (the merged lock scan), `classify`
//! (holder/driver selection), `prs` (one listing, binding,
//! mergeable filter), `scope` (config paths + crown scope), `queues`
//! (the lane parser + the eleven-queue build). This parent holds the
//! shared value vocabulary, the options, the collection orchestration,
//! and the board-shape tests.

mod budget;
mod claims;
mod classify;
mod prs;
mod queues;
mod scope;

pub(crate) use queues::not_read_status;

use crate::graph_store;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub(crate) use budget::{fno_py_cmd, now_secs_board, run_json, Budget, HAND_RUN_BUDGET_MS};
pub(crate) use claims::read_claims;
pub(crate) use classify::read_claimed_nodes;
pub(crate) use prs::read_prs;
pub(crate) use queues::{
    build_board, parse_lane, queue_json, read_blocked_rows, BoardInputs, Queue,
};
pub(crate) use scope::{
    autonomous_merge_enabled, blocked_child_grace_minutes, compile_scope_ids, graph_json_path,
    operator_lane_path, parse_manifest, project_map,
};

/// Priorities a king treats as its own work. Lower bands are the operator's.
pub(crate) const KING_PRIORITIES: [&str; 2] = ["p0", "p1"];

/// Claim states that mean the lock outlived its holder.
pub(crate) const DEAD_CLAIM_STATES: [&str; 2] = ["stale", "corrupted"];

pub(crate) const TERMINAL_RUNGS: [&str; 2] = ["done", "superseded"];
pub(crate) const LEGACY_DEFER_PREFIX: &str = "deferred:";

/// The literal commands a reader can re-run; they ARE the checkability
/// property, so they sit beside the readers (board.py spelled them identically).
pub(crate) const SRC_UNDISPATCHED: &str = "fno backlog undispatched --json";
/// The unplanned queue's ready source answers in-process now; the label
/// names the function, the way `agents claim list` labels its source.
pub(crate) const SRC_READY: &str = "backlog_ready::select (-A)";
pub(crate) const SRC_WORKED: &str = "fno backlog worked --json";
pub(crate) const SRC_CLAIMS: &str = "fno agents claim list -J --include-stale --prefix node:";
pub(crate) const SRC_PRS: &str =
    "gh pr list --state open --json number,title,mergeable,statusCheckRollup,headRefName,url";
pub(crate) const SRC_PR_NODES: &str = "gh pr list --state open --json number,title,mergeable,statusCheckRollup,headRefName,url + fno backlog get <id>";
pub(crate) const SRC_QUESTIONS: &str = "fno inbox outstanding --json";
pub(crate) const SRC_NEEDS: &str = "fno agents needs --json";
pub(crate) const SRC_DISTRESS: &str =
    "~/.fno/events.jsonl (blocked rows) + bus/messages.jsonl + fno agents distress-verdicts";

fn worked_node_ids(read: &SourceRead) -> HashSet<String> {
    read.payload
        .as_ref()
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|row| row.get("id").and_then(Value::as_str).map(str::to_string))
        .collect()
}

// ---------------------------------------------------------------------------
// SourceRead: one source's answer, or the reason there is no answer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub(crate) struct SourceRead {
    pub(crate) payload: Option<Value>,
    pub(crate) error: Option<String>,
    /// True when the read died at its budget slice, not at a source failure.
    pub(crate) over_budget: bool,
}

impl SourceRead {
    pub(crate) fn ok(payload: Value) -> Self {
        SourceRead {
            payload: Some(payload),
            error: None,
            over_budget: false,
        }
    }
    pub(crate) fn err(msg: impl Into<String>) -> Self {
        SourceRead {
            payload: None,
            error: Some(msg.into()),
            over_budget: false,
        }
    }
    pub(crate) fn over_budget(msg: impl Into<String>) -> Self {
        SourceRead {
            payload: None,
            error: Some(msg.into()),
            over_budget: true,
        }
    }
    /// Re-wrap a failed read under a new message, keeping its verdict: a
    /// composition site must not flatten a budget kill into a plain failure.
    pub(crate) fn rewrap(&self, msg: impl Into<String>) -> Self {
        SourceRead {
            payload: None,
            error: Some(msg.into()),
            over_budget: self.over_budget,
        }
    }
    pub(crate) fn is_ok(&self) -> bool {
        self.error.is_none()
    }
    pub(crate) fn rows(&self) -> Vec<Value> {
        match &self.payload {
            Some(Value::Array(rows)) => rows.clone(),
            _ => Vec::new(),
        }
    }
    /// The nested-shape half of the degrade-not-crash promise (board.py
    /// `_as_dict`): a stream that changed shape degrades that stream, never
    /// the whole board.
    pub(crate) fn dict(&self) -> Map<String, Value> {
        match &self.payload {
            Some(Value::Object(m)) => m.clone(),
            _ => Map::new(),
        }
    }
}

pub(crate) fn s_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

pub(crate) fn s_i64(v: &Value, key: &str) -> Option<i64> {
    v.get(key).and_then(Value::as_i64)
}

/// Python `bool()` over a JSON value: null/false/empty-string/zero/empty
/// container are false. The classify port decides `completed`/`has_pr`/
/// `batch_owner` the way the Python `bool(entry.get(...))` did, so a legacy
/// empty-string `completed_at` or a zero `pr_number` reads as absent, exactly
/// as the retired module read it.
pub(crate) fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(st) => !st.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

pub(crate) fn as_int(v: &Value) -> i64 {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Collection
// ---------------------------------------------------------------------------

/// The `fno-agents board` entry point: parse flags, print the payload, exit
/// with the board's own exit_code. Kept beside the parity sets' reach (an
/// `==` arm in client.rs), so no advertised verb is added.
pub fn run_board(args: &[String]) -> i32 {
    let mut opts = BoardOpts::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--json" | "-J" => {}
            "--budget-ms" => match it.next().and_then(|v| v.parse::<u64>().ok()) {
                Some(v) => opts.budget_ms = v,
                None => {
                    eprintln!("fno-agents board: --budget-ms needs a millisecond integer");
                    return 2;
                }
            },
            "--max-pr-reads" => match it.next().and_then(|v| v.parse::<usize>().ok()) {
                Some(v) => opts.max_pr_reads = v,
                None => {
                    eprintln!("fno-agents board: --max-pr-reads needs an integer");
                    return 2;
                }
            },
            "--state" => match it.next() {
                Some(v) => opts.state_path = Some(PathBuf::from(v)),
                None => {
                    eprintln!("fno-agents board: --state needs a path");
                    return 2;
                }
            },
            other => {
                eprintln!("fno-agents board: unknown flag {other}");
                return 2;
            }
        }
    }
    let payload = read_board(&opts);
    println!(
        "{}",
        serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into())
    );
    payload
        .get("exit_code")
        .and_then(Value::as_i64)
        .map(|c| c as i32)
        .unwrap_or(1)
}

pub struct BoardOpts {
    pub budget_ms: u64,
    pub max_pr_reads: usize,
    /// King manifest whose `scope` bounds the board.
    pub state_path: Option<PathBuf>,
    /// The directory every config-tier, claims-root, and project-journal
    /// resolution anchors on. `None` means the process cwd (the hand-run CLI
    /// case); an IN-PROCESS caller such as loopcheck passes its `--cwd` here,
    /// because the old subprocess board ran with the king session's cwd and
    /// the calling process's cwd is not guaranteed to be the same directory.
    pub cwd: Option<PathBuf>,
}

impl Default for BoardOpts {
    fn default() -> Self {
        BoardOpts {
            budget_ms: HAND_RUN_BUDGET_MS,
            max_pr_reads: 20,
            state_path: None,
            cwd: None,
        }
    }
}

/// Read the whole board. Never panics on a source; every failure lands in its
/// queue's error and the payload still answers.
pub fn read_board(opts: &BoardOpts) -> Value {
    let cwd = opts
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let mut budget = Budget::new(opts.budget_ms);
    let mut sources: Map<String, Value> = Map::new();
    let mut warnings: Vec<String> = Vec::new();

    let mark =
        |sources: &mut Map<String, Value>, name: &str, read: &SourceRead, truncated: bool| {
            sources.insert(
                name.to_string(),
                json!({
                    "ok": read.is_ok(),
                    "truncated": truncated,
                    "error": read.error.clone().unwrap_or_default(),
                }),
            );
        };

    // The graph: ONE read, shared by undispatched, claimed-node lookups, PR
    // binding, and crown scope. `read_defaulted_opts(path, false, false)` is
    // exactly the keeper's `read_strict` (defaults applied, diagnosis without
    // a .bak), which is what the Python board's read_graph_strict runs.
    let graph_path = graph_json_path(&cwd);
    let entries: Option<Vec<Value>> =
        match graph_store::read_defaulted_opts(&graph_path, false, false) {
            Ok(e) => Some(e),
            Err(e) => {
                warnings.push(format!("graph unreadable: {e}"));
                None
            }
        };

    let spent = |sources: &mut Map<String, Value>, name: &str, budget: &Budget| {
        let err = budget.spent_error();
        sources.insert(
            name.to_string(),
            json!({"ok": false, "truncated": true, "error": err}),
        );
    };

    // The reference charged slices sequentially; this collector charges them
    // in the same order but runs the four subprocess reads CONCURRENTLY, for
    // the reason the Python board ran its six sources on a thread pool:
    // sequential execution lets the slowest source (gh pr list under fleet
    // load, measured 28s here) starve every queue behind it. The deadline is
    // still the ONE bound: every slice derives from the same total, and the
    // slowest source now bounds the wall, not the sum.
    let s_undispatched = budget.start("backlog undispatched");
    let s_claims = budget.start("agents claim list");
    let s_prs = budget.start(SRC_PRS);
    let s_stalled = budget.start("stalled_holder lookups");
    let s_ready = budget.start(SRC_READY);
    let s_worked = budget.start(SRC_WORKED);
    let s_outstanding = budget.start(SRC_QUESTIONS);
    let s_needs = budget.start(SRC_NEEDS);
    let s_blocked_child = budget.start(SRC_DISTRESS);

    // Mostly in-process: graph already read; claims scan, claimed-node lookups,
    // the needs fold, and the lane file. Undispatched is the exception and
    // spawns, for the reason given at its own block below.
    let claims = match s_claims {
        None => {
            spent(&mut sources, "claims", &budget);
            SourceRead::err(budget.spent_error())
        }
        Some(_) => {
            let read = read_claims(&cwd);
            mark(&mut sources, "claims", &read, false);
            read
        }
    };
    // Undispatched is the one source this board SHELLS OUT for rather than
    // classifying in-process. The in-process copy was a declared pure port of
    // `backlog/undispatched.classify_planned_unclaimed`, and a port has to be
    // re-ported every time the original moves. It did not move for a long
    // time, then the Python side adopted the shared selection key and the two
    // named different next nodes on the same graph. One implementation, at the
    // cost of one subprocess inside the slice the source already had.
    let undispatched = match s_undispatched {
        None => {
            spent(&mut sources, "undispatched", &budget);
            SourceRead::err(budget.spent_error())
        }
        Some(slice) => {
            let mut cmd = fno_py_cmd();
            cmd.extend([
                "backlog".to_string(),
                "undispatched".to_string(),
                "--json".to_string(),
            ]);
            let read = run_json(cmd, &cwd, slice);
            mark(&mut sources, "undispatched", &read, false);
            let rows = read
                .payload
                .as_ref()
                .and_then(|r| r.get("rows").and_then(Value::as_array).cloned());
            match rows {
                Some(rows) => SourceRead::ok(Value::Array(rows)),
                None => read,
            }
        }
    };

    // Claimed nodes: from the locks to the rows, one graph read.
    let (claimed_nodes, holders, claimed_warnings) = match s_stalled {
        None => {
            spent(&mut sources, "claimed_nodes", &budget);
            (
                SourceRead::err(budget.spent_error()),
                Vec::new(),
                Vec::new(),
            )
        }
        Some(_) => {
            let (read, mut holders, w) = read_claimed_nodes(&claims, entries.as_deref());
            // The probe feed must also measure DEAD-stated claim holders:
            // the reordered readers ask the holder before honoring the clock,
            // and a holder nobody probed always reads inactive.
            for h in classify::dead_claim_holders(&claims) {
                if !holders.contains(&h) {
                    holders.push(h);
                }
            }
            mark(&mut sources, "claimed_nodes", &read, false);
            (read, holders, w)
        }
    };
    warnings.extend(claimed_warnings);

    // The reads that can take real wall time run concurrently: gh pr
    // list, `fno inbox outstanding`, the batched truth
    // probe, the ready selection (in-process; its plan-document disk reads
    // still cost the slice), and the needs fold (in-process, but its refused-worker leg batch
    // probes the whole registry and measured ~7s on a busy fleet). Their
    // slices were derived above in the reference's order.
    let entries_ref = entries.as_deref();
    let cwd_for_threads = cwd.clone();
    let (
        prs,
        pr_nodes,
        pr_warnings,
        prs_truncated,
        worked,
        ready,
        outstanding,
        needs,
        holder_activity,
        holder_activity_error,
    ) = std::thread::scope(|s| {
        // The worked read is a full fno-py cold start plus fleet roster read,
        // so it rides the concurrent section too: its join waits below, after
        // the other subprocess threads are already running, and only the
        // ready thread (its one consumer) waits for the result.
        let t_worked = s_worked.map(|slice| {
            let cwd = cwd_for_threads.clone();
            s.spawn(move || {
                let mut cmd = fno_py_cmd();
                cmd.extend([
                    "backlog".to_string(),
                    "worked".to_string(),
                    "--json".to_string(),
                ]);
                run_json(cmd, &cwd, slice)
            })
        });
        let t_prs = s_prs.map(|slice| {
            let cwd = cwd_for_threads.clone();
            s.spawn(move || {
                let (prs, pr_nodes, w) = read_prs(&cwd, slice, opts.max_pr_reads, entries_ref);
                let truncated = w.iter().any(|x| x.contains("hit its"));
                (prs, pr_nodes, w, truncated)
            })
        });
        let worked = match t_worked {
            None => SourceRead::err(budget.spent_error()),
            Some(h) => h
                .join()
                .unwrap_or(SourceRead::err("worked: reader panicked")),
        };
        mark(&mut sources, "worked", &worked, false);
        let worked_ids = worked_node_ids(&worked);
        let t_ready = s_ready.map(|_slice| {
            let entries = entries_ref.map(|e| e.to_vec());
            let cwd = cwd_for_threads.clone();
            let worked_ids = worked_ids.clone();
            s.spawn(move || {
                // In-process now: the admission decision lives in this
                // binary (backlog_ready::select) and reads the graph the
                // board already loaded, so the source costs plan-document
                // disk reads, not an interpreter cold start. The budget
                // slice stays - the source still costs wall time.
                let Some(entries) = entries else {
                    return SourceRead::err("graph unreadable: no entries for ready selection");
                };
                // Strict claims read: an unreadable root is unknown claim
                // state, surfaced as a source error rather than an empty
                // set that would re-dispatch held work.
                let claimed_records = crate::claims::list(Some("node:"), None, false);
                let Ok(claim_records) = claimed_records else {
                    return SourceRead::err(format!(
                        "claims unreadable: {}",
                        claimed_records.err().unwrap_or_default()
                    ));
                };
                let mut claimed = std::collections::BTreeSet::new();
                for rec in claim_records {
                    if let Some(id) = rec.key.strip_prefix("node:") {
                        claimed.insert(id.to_string());
                    }
                }
                claimed.extend(worked_ids);
                let opts = crate::backlog_ready::ReadyOpts {
                    all: true,
                    repo_root: crate::paths::canonical_repo_root(&cwd)
                        .map(|p| p.display().to_string()),
                    staleness_days: crate::backlog_ready::configured_staleness_days(
                        &cwd.join(".fno"),
                    ),
                    claimed,
                    now_ms: crate::claims::now_ms(),
                    ..Default::default()
                };
                match crate::backlog_ready::select(&entries, &opts) {
                    Ok(reply) => SourceRead::ok(Value::Array(reply.rows)),
                    Err(_) => SourceRead::err("ready: parent scope did not resolve"),
                }
            })
        });
        let t_outstanding = s_outstanding.map(|slice| {
            let cwd = cwd_for_threads.clone();
            s.spawn(move || {
                let mut cmd = fno_py_cmd();
                cmd.extend(
                    ["inbox", "outstanding", "--json"]
                        .iter()
                        .map(|s| s.to_string()),
                );
                run_json(cmd, &cwd, slice)
            })
        });
        // ONE batched truth probe for every holder the king cares about (the
        // single-transcript-reader constraint; a probe per holder would pay
        // one interpreter cold start each).
        let t_truth = if holders.is_empty() {
            None
        } else {
            let tokens: Vec<String> = holders
                .iter()
                .map(|h| {
                    h.split_once(':')
                        .map(|(_, t)| t.to_string())
                        .unwrap_or_else(|| h.clone())
                })
                .collect();
            Some(s.spawn(move || crate::truth_probe::family1_truth_probe_many_checked(&tokens)))
        };
        // The needs fold rides a thread too: in-process, but its
        // refused-worker leg batch probes the whole registry and measured
        // ~7s on a busy fleet, which no longer sits on the critical path.
        let t_needs = s_needs.map(|_slice| {
            let cwd = cwd_for_threads.clone();
            s.spawn(move || {
                let home = crate::paths::AgentsHome::from_env();
                let (mut event_paths, default_ledger) = default_needs_sources(&home);
                // The canonical checkout's journal, exactly as `run_needs`
                // adds it: a question asked from a worktree writes the
                // CANONICAL .fno/events.jsonl, never the worktree's. The
                // project journal anchors on the BOARD's cwd (the caller's
                // --cwd for an in-process reader), never the process cwd.
                event_paths[0] = cwd.join(".fno").join("events.jsonl");
                if let Some(root) = crate::paths::canonical_repo_root(&cwd) {
                    let canonical_events = root.join(".fno").join("events.jsonl");
                    let cwd_events = cwd.join(".fno").join("events.jsonl");
                    if canonical_events != cwd_events && !event_paths.contains(&canonical_events) {
                        event_paths.push(canonical_events);
                    }
                }
                let since = now_secs_board().saturating_sub(crate::needs::DEFAULT_WINDOW_SECS);
                crate::needs::collect_needs_items(
                    &home,
                    &event_paths,
                    &default_ledger,
                    since,
                    crate::needs::DEFAULT_FIRES_FLOOR,
                    &cwd,
                )
            })
        });

        let (prs, pr_nodes, pr_warnings, prs_truncated) = match t_prs {
            None => {
                let err = budget.spent_error();
                (
                    SourceRead::err(err.clone()),
                    SourceRead::err(err),
                    Vec::new(),
                    true,
                )
            }
            Some(h) => h.join().unwrap_or_else(|_| {
                (
                    SourceRead::err("prs: reader panicked"),
                    SourceRead::err("undriven_pr: reader panicked"),
                    Vec::new(),
                    false,
                )
            }),
        };
        let ready = match t_ready {
            None => SourceRead::err(budget.spent_error()),
            Some(h) => h
                .join()
                .unwrap_or(SourceRead::err("ready: reader panicked")),
        };
        let outstanding = match t_outstanding {
            None => SourceRead::err(budget.spent_error()),
            Some(h) => h
                .join()
                .unwrap_or(SourceRead::err("outstanding: reader panicked")),
        };
        let outstanding = if outstanding.is_ok() {
            SourceRead::ok(outstanding.payload.unwrap_or(json!({})))
        } else {
            outstanding
        };
        let needs = match t_needs {
            None => SourceRead::err(budget.spent_error()),
            // A panicked fold must read UNREADABLE, never ok-empty: an
            // empty needs stream and a stream that never answered are
            // different boards, and the queue's loudness is the only
            // place the difference survives.
            Some(h) => h
                .join()
                .map(|items| SourceRead::ok(serde_json::to_value(&items).unwrap_or(json!([]))))
                .unwrap_or_else(|_| SourceRead::err("needs: reader panicked")),
        };
        let (holder_activity, holder_activity_error): (
            HashMap<String, crate::truth_probe::TruthProbe>,
            Option<String>,
        ) = match t_truth {
            None => (HashMap::new(), None),
            Some(h) => match h.join() {
                Ok(Ok(map)) => (map, None),
                // A timed-out batch is UNREADABLE, not empty: an empty map
                // read as "every holder answered nothing" is how live workers
                // rendered stalled (x-db9c).
                Ok(Err(e)) => (HashMap::new(), Some(e)),
                Err(_) => (
                    HashMap::new(),
                    Some("truth probe: reader panicked".to_string()),
                ),
            },
        };
        (
            prs,
            pr_nodes,
            pr_warnings,
            prs_truncated,
            worked,
            ready,
            outstanding,
            needs,
            holder_activity,
            holder_activity_error,
        )
    });
    warnings.extend(pr_warnings);
    let prs_truncated = prs_truncated || !prs.is_ok();
    mark(&mut sources, "prs", &prs, prs_truncated);
    mark(&mut sources, "ready", &ready, false);
    mark(&mut sources, "outstanding", &outstanding, false);
    sources.insert(
        "holder_activity".to_string(),
        json!({
            "ok": holder_activity_error.is_none(),
            "truncated": false,
            "error": holder_activity_error.clone().unwrap_or_default(),
        }),
    );

    // Lane: a file read; there is no verb behind it.
    let lane_path = operator_lane_path(&cwd);
    let lane = match parse_lane(&lane_path) {
        Err(e) => SourceRead::err(e),
        Ok(items) => SourceRead::ok(Value::Array(
            items
                .iter()
                .map(|i| {
                    json!({
                        "text": i.text,
                        "node": i.node,
                        "parked": i.parked,
                        "done": i.done,
                        "line": i.line,
                    })
                })
                .collect(),
        )),
    };
    mark(&mut sources, "lane", &lane, false);

    // Scope.
    let mut scope_ids: Option<HashSet<String>> = None;
    let mut crown_scope: Option<String> = None;
    if let Some(state_path) = &opts.state_path {
        let manifest = parse_manifest(state_path);
        let scope = manifest.get("scope").cloned().unwrap_or_default();
        if scope.is_empty() {
            return json!({
                "actionable": 1,
                "unreadable": 1,
                "queues": [queue_json(&Queue {
                    name: "scope",
                    source: state_path.display().to_string(),
                    status: "unreadable",
                    error: "king manifest has no scope".to_string(),
                    count: -1,
                    rows: Vec::new(),
                    actionable: true,
                    note: String::new(),
                    verb: "",
                })],
                "warnings": warnings,
                "exit_code": 1,
                "sources": Value::Object(sources),
            });
        }
        crown_scope = Some(scope.clone());
        let projects = project_map(&cwd);
        match compile_scope_ids(&scope, entries.as_deref().unwrap_or(&[]), &projects) {
            Ok(ids) => scope_ids = Some(ids),
            Err(e) => {
                return json!({
                    "actionable": 1,
                    "unreadable": 1,
                    "queues": [queue_json(&Queue {
                        name: "scope",
                        source: format!("king manifest scope {scope}"),
                        status: "unreadable",
                        error: e,
                        count: -1,
                        rows: Vec::new(),
                        actionable: true,
                        note: String::new(),
                        verb: "",
                    })],
                    "warnings": warnings,
                    "exit_code": 1,
                    "sources": Value::Object(sources),
                });
            }
        }
    }

    // Mirrors `bus/log.py::bus_dir`'s env overrides only, not its
    // `config.paths.bus_dir` template override - a documented gap: an
    // operator who relocates the bus via config gets a mail-check that
    // never sees mail there, degrading to "still shows unanswered" rather
    // than a wrong answer.
    fn bus_live_log_path() -> PathBuf {
        if let Some(dir) = std::env::var("FNO_BUS_DIR").ok().filter(|v| !v.is_empty()) {
            return PathBuf::from(dir).join("messages.jsonl");
        }
        if let Some(root) = std::env::var("FNO_INBOX_ROOT")
            .ok()
            .filter(|v| !v.is_empty())
        {
            return PathBuf::from(root).join(".bus").join("messages.jsonl");
        }
        let home = crate::paths::AgentsHome::from_env();
        home.root()
            .parent()
            .unwrap_or_else(|| home.root())
            .join("bus")
            .join("messages.jsonl")
    }

    // Blocked child: read the global distress journal and resolve every
    // candidate's answered/unanswered state HERE, where claims, the graph,
    // and the verdict subprocess already live - build_board only
    // scope-filters and renders what this collects (x-3ecf). Gated on
    // `s_blocked_child` like every other source, so an exhausted board
    // budget skips it rather than running an unbounded read anyway - the
    // ONE bound this module's own contract promises. Wrapped in
    // catch_unwind for the same reason the needs thread's `.join()` is:
    // `AgentsHome::from_env()` panics under a test process with no
    // declared hermetic root (paths.rs), and this function's own contract
    // is "never panics on a source" (see doc comment above).
    let blocked_child = match s_blocked_child {
        None => {
            spent(&mut sources, "blocked_child", &budget);
            SourceRead::err(budget.spent_error())
        }
        Some(slice) => {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let home = crate::paths::AgentsHome::from_env();
                let journal = crate::daemon::global_events_path(&home);
                match read_blocked_rows(&journal) {
                    Err(e) => SourceRead::err(e),
                    Ok(rows) => {
                        let claim_rows = claims.rows();
                        let claim_state_by_node: HashMap<String, String> = claim_rows
                            .iter()
                            .filter_map(|row| {
                                let node = s_str(row, "key")?.strip_prefix("node:")?;
                                Some((
                                    node.to_string(),
                                    s_str(row, "state").unwrap_or("").to_string(),
                                ))
                            })
                            .collect();
                        let status_by_node: HashMap<String, String> = entries
                            .as_deref()
                            .unwrap_or(&[])
                            .iter()
                            .filter_map(|n| {
                                Some((
                                    s_str(n, "id")?.to_string(),
                                    s_str(n, "status").unwrap_or("").to_string(),
                                ))
                            })
                            .collect();
                        let candidates = queues::resolve_blocked_child_candidates(
                            rows,
                            &claim_state_by_node,
                            &status_by_node,
                            blocked_child_grace_minutes(&cwd),
                            now_secs_board() as i64,
                        );
                        if candidates.is_empty() {
                            SourceRead::ok(Value::Array(Vec::new()))
                        } else {
                            let cutoffs: HashMap<String, String> = candidates
                                .iter()
                                .map(|(row, _)| (row.session.clone(), row.ts.clone()))
                                .collect();
                            let answered =
                                queues::mail_answered_since(&bus_live_log_path(), &cutoffs);

                            let sessions: Vec<Value> = candidates
                                .iter()
                                .map(|(row, _)| Value::String(row.session.clone()))
                                .collect();
                            let mut cmd = fno_py_cmd();
                            cmd.extend([
                                "agents".to_string(),
                                "distress-verdicts".to_string(),
                                "--sessions".to_string(),
                                serde_json::to_string(&sessions)
                                    .unwrap_or_else(|_| "[]".to_string()),
                            ]);
                            let verdict_payload = run_json(cmd, &cwd, slice);
                            let verdicts: HashMap<String, String> = candidates
                                .iter()
                                .filter_map(|(row, _)| {
                                    verdict_payload
                                        .payload
                                        .as_ref()
                                        .and_then(|p| p.get(&row.session))
                                        .and_then(Value::as_str)
                                        .map(|v| (row.session.clone(), v.to_string()))
                                })
                                .collect();
                            SourceRead::ok(Value::Array(queues::filter_unanswered_by_mail(
                                candidates, &answered, &verdicts,
                            )))
                        }
                    }
                }
            }));
            let read = result.unwrap_or_else(|_| SourceRead::err("blocked_child: reader panicked"));
            mark(&mut sources, "blocked_child", &read, false);
            read
        }
    };

    let inputs = BoardInputs {
        ready,
        claims,
        worked,
        claimed_nodes,
        holder_activity,
        holder_activity_error,
        prs,
        pr_nodes,
        outstanding,
        needs,
        lane,
        undispatched,
        blocked_child,
        entries,
        warnings,
        autonomous_merge: autonomous_merge_enabled(&cwd),
        scope_ids,
        crown_scope,
    };
    let mut payload = build_board(&inputs);
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("sources".to_string(), Value::Object(sources));
    }
    payload
}

/// The needs verb's default sources (needs.default_sources): project + global
/// events + questions, and the ledger.
pub(crate) fn default_needs_sources(home: &crate::paths::AgentsHome) -> (Vec<PathBuf>, PathBuf) {
    let fno_dir = home
        .root()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(".fno"));
    let global_events = fno_dir.join("events.jsonl");
    let questions = fno_dir.join("questions.jsonl");
    let project_events = PathBuf::from(".fno").join("events.jsonl");
    let ledger = fno_dir.join("ledger.json");
    (vec![project_events, global_events, questions], ledger)
}

#[cfg(test)]
mod tests {
    use super::classify::node_driver;
    use super::*;

    fn ok_read(payload: Value) -> SourceRead {
        SourceRead::ok(payload)
    }

    fn inputs_with(ready: Value, claims: Value, claimed_nodes: Value) -> BoardInputs {
        BoardInputs {
            ready: ok_read(ready),
            claims: ok_read(claims),
            worked: ok_read(Value::Array(Vec::new())),
            claimed_nodes: ok_read(claimed_nodes),
            holder_activity: HashMap::new(),
            holder_activity_error: None,
            prs: ok_read(Value::Array(Vec::new())),
            pr_nodes: ok_read(Value::Array(Vec::new())),
            outstanding: ok_read(json!({})),
            needs: ok_read(Value::Array(Vec::new())),
            blocked_child: ok_read(Value::Array(Vec::new())),
            lane: ok_read(Value::Array(Vec::new())),
            undispatched: ok_read(Value::Array(Vec::new())),
            entries: None,
            warnings: Vec::new(),
            autonomous_merge: false,
            scope_ids: None,
            crown_scope: None,
        }
    }

    #[test]
    fn worked_source_extracts_node_ids() {
        let read = SourceRead::ok(json!([{
            "id": "x-live",
            "workers": ["bp-worker"],
        }]));

        assert_eq!(
            worked_node_ids(&read),
            HashSet::from(["x-live".to_string()])
        );
    }

    #[test]
    fn unplanned_queue_is_unreadable_when_worked_source_fails() {
        let mut inputs = inputs_with(
            json!([{"id": "x-live", "priority": "p0", "plan_path": null}]),
            json!([]),
            json!([]),
        );
        inputs.worked = SourceRead::err("roster timeout");

        let board = build_board(&inputs);
        let queue = board["queues"]
            .as_array()
            .unwrap()
            .iter()
            .find(|queue| queue["name"] == "unplanned")
            .unwrap();
        assert_eq!(queue["status"], "unreadable");
        assert!(queue["rows"].as_array().unwrap().is_empty());
        assert_eq!(queue["error"], "roster timeout");
    }

    #[test]
    fn unplanned_note_names_the_batch_and_undispatched_names_the_target() {
        // x-c1c7: a rule without a number is advice nobody applies; the
        // queue a king dispatches from names the verb, never the blueprint.
        let inputs = inputs_with(
            json!([{"id": "x-1234", "priority": "p0"}]),
            json!([]),
            json!([]),
        );
        let board = build_board(&inputs);
        let queues = board.get("queues").and_then(Value::as_array).unwrap();
        let unplanned = queues.iter().find(|q| q["name"] == "unplanned").unwrap();
        let note = unplanned["note"].as_str().unwrap();
        assert!(!note.is_empty());
        assert!(note.contains('3') || note.to_lowercase().contains("three"));
        let undispatched = queues.iter().find(|q| q["name"] == "undispatched").unwrap();
        assert_eq!(undispatched["verb"], "/fno:target");
        assert!(!undispatched["note"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("blueprint"));
    }

    #[test]
    fn a_budget_kill_and_a_failed_exit_tally_apart_but_both_hold_exit_code_1() {
        let mut inputs = inputs_with(json!([]), json!([]), json!([]));
        // entries: None reads as an unreadable queue of its own; an empty
        // list keeps the fixture's failure count at exactly the two legs.
        inputs.entries = Some(Vec::new());
        inputs.undispatched = SourceRead::over_budget(
            "fno backlog undispatched --json: killed at its 28.5s slice of the board budget; the source did not fail",
        );
        inputs.prs = SourceRead::err("exit 1: gh pr list failed");
        let board = build_board(&inputs);
        assert_eq!(board["over_budget"], 1);
        assert_eq!(board["unreadable"], 1);
        assert_eq!(board["exit_code"], 1);
        let queues = board["queues"].as_array().unwrap();
        let undispatched = queues.iter().find(|q| q["name"] == "undispatched").unwrap();
        assert_eq!(undispatched["status"], "over_budget");
        let prs = queues.iter().find(|q| q["name"] == "mergeable_pr").unwrap();
        assert_eq!(prs["status"], "unreadable");
        assert_eq!(prs["count"], Value::Null);
    }

    #[test]
    fn stalled_holder_excludes_done_nodes() {
        let node = json!({
            "id": "x-doen",
            "priority": "p0",
            "status": "done",
            "completed_at": "2026-08-21T03:16:00Z",
        });
        let claims = json!([{"key": "node:x-doen", "state": "live", "holder": "h"}]);
        let inputs = inputs_with(json!([]), claims, json!([node]));
        let board = build_board(&inputs);
        let queues = board.get("queues").and_then(Value::as_array).unwrap();
        let stalled = queues
            .iter()
            .find(|q| q["name"] == "stalled_holder")
            .unwrap();
        assert_eq!(stalled["rows"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn stalled_holder_still_names_a_live_open_node() {
        let node = json!({"id": "x-open", "priority": "p0", "status": "in_progress"});
        let claims = json!([{"key": "node:x-open", "state": "live", "holder": "h"}]);
        let mut inputs = inputs_with(json!([]), claims, json!([node]));
        // The probe ANSWERED for the holder - `unknown` is the batch's
        // per-handle "resolves to nothing" shape - so the row reads stalled.
        // With no entry at all the holder reads unmeasured, and an unmeasured
        // holder belongs to no row.
        inputs.holder_activity.insert(
            "h".to_string(),
            crate::truth_probe::TruthProbe {
                state: "unknown".to_string(),
                provider_refusal: None,
                harness_title: None,
                reachability: None,
                basis: None,
                last_activity_age_s: Some(30.0),
                last_event_at: None,
                last_message: None,
                observed_model: Value::Null,
            },
        );
        let board = build_board(&inputs);
        let queues = board.get("queues").and_then(Value::as_array).unwrap();
        let stalled = queues
            .iter()
            .find(|q| q["name"] == "stalled_holder")
            .unwrap();
        let ids: Vec<&str> = stalled["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["id"].as_str())
            .collect();
        assert_eq!(ids, vec!["x-open"]);
        // AC8-HP: the row names the worker behind the marker, and the source
        // hands the reader the peek verb for THAT worker, not the raw holder
        // string no resolver accepts.
        let row = &stalled["rows"].as_array().unwrap()[0];
        assert_eq!(row["worker"], "h");
        assert!(stalled["source"]
            .as_str()
            .unwrap()
            .contains("peek <worker>"));
    }

    #[test]
    fn x_dead_contained_nodes_reach_neither_queue() {
        // Task 1.4b (the x-58a5 shape): a node with `contained_in` set has an
        // owner by definition and never dispatches alone, so `none` - the
        // word that fills unheld_progress and undriven_pr - is not an
        // available verdict for it. One check inside node_driver drops it
        // from both queues at once.
        let mut inputs = inputs_with(json!([]), json!([]), json!([]));
        inputs.entries = Some(vec![
            json!({
                "id": "x-58a5", "priority": "p1", "status": "in_progress",
                "title": "contained work", "contained_in": "x-b7f8",
            }),
            json!({
                "id": "x-contained-pr", "priority": "p1", "status": "in_progress",
                "title": "contained with a pr", "contained_in": "x-owner", "pr_number": 42,
            }),
        ]);
        inputs.pr_nodes = ok_read(json!([
            json!({"id": "x-contained-pr", "priority": "p1", "pr_number": 42}),
        ]));
        let board = build_board(&inputs);
        let queues = board.get("queues").and_then(Value::as_array).unwrap();
        for name in ["unheld_progress", "undriven_pr"] {
            let q = queues.iter().find(|q| q["name"] == name).unwrap();
            assert_eq!(
                q["rows"].as_array().map(|rows| rows.len()).unwrap_or(0),
                0,
                "{q}"
            );
        }
    }

    #[test]
    fn unheld_progress_names_a_claim_free_in_progress_row() {
        // x-add3 sat in_progress 25 minutes with a free claim and a dead
        // worker while every queue read clean; the status stamp is never
        // revoked, so this queue is the only one that can carry it.
        let mut inputs = inputs_with(json!([]), json!([]), json!([]));
        inputs.entries = Some(vec![json!({
            "id": "x-add3",
            "priority": "p1",
            "status": "in_progress",
            "title": "dead handoff",
        })]);
        let board = build_board(&inputs);
        let queues = board.get("queues").and_then(Value::as_array).unwrap();
        let unheld = queues
            .iter()
            .find(|q| q["name"] == "unheld_progress")
            .unwrap();
        let rows = unheld["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{unheld}");
        assert_eq!(rows[0]["id"], "x-add3");
        assert!(rows[0].get("claim_state").is_none(), "{:?}", rows[0]);
    }

    #[test]
    fn unheld_progress_omits_claimed_pr_bound_and_ready_rows() {
        let mut inputs = inputs_with(
            json!([]),
            json!([{"key": "node:x-held", "state": "live", "holder": "h"}]),
            json!([]),
        );
        inputs.entries = Some(vec![
            json!({"id": "x-held", "priority": "p1", "status": "in_progress"}),
            json!({"id": "x-prbd", "priority": "p1", "status": "in_progress", "pr_number": 42}),
            json!({"id": "x-rdy", "priority": "p1", "status": "ready"}),
        ]);
        let board = build_board(&inputs);
        let queues = board.get("queues").and_then(Value::as_array).unwrap();
        let unheld = queues
            .iter()
            .find(|q| q["name"] == "unheld_progress")
            .unwrap();
        assert_eq!(unheld["rows"].as_array().unwrap().len(), 0, "{unheld}");
    }

    #[test]
    fn unheld_progress_omits_a_crowned_epic_but_keeps_a_dead_leaf() {
        // x-26e5: a king holds a crown, never a claim, so the epic it reigns
        // over read claim-free and held the stop hook open on a row no verb
        // could clear. The crown is that epic's driver. An in-scope leaf with
        // no claim is still a dead handoff the king must redispatch.
        let mut inputs = inputs_with(json!([]), json!([]), json!([]));
        inputs.entries = Some(vec![
            json!({"id": "x-epic", "priority": "p1", "status": "in_progress", "type": "epic"}),
            json!({"id": "x-leaf", "priority": "p1", "status": "in_progress", "parent": "x-epic"}),
        ]);
        inputs.scope_ids = Some(
            ["x-epic", "x-leaf"]
                .into_iter()
                .map(str::to_string)
                .collect(),
        );
        inputs.crown_scope = Some("x-epic".to_string());
        let board = build_board(&inputs);
        let queues = board.get("queues").and_then(Value::as_array).unwrap();
        let unheld = queues
            .iter()
            .find(|q| q["name"] == "unheld_progress")
            .unwrap();
        let ids: Vec<&str> = unheld["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["id"].as_str())
            .collect();
        assert_eq!(ids, vec!["x-leaf"], "{unheld}");
    }

    #[test]
    fn unheld_progress_omits_an_epic_with_a_live_claimed_child() {
        // An epic goes in_progress because its children are worked. The epic
        // never takes a claim or opens a PR, so the leaf test flagged every
        // healthy epic forever. A live child claim is the epic's driver.
        let mut inputs = inputs_with(
            json!([]),
            json!([{"key": "node:x-child", "state": "live", "holder": "h"}]),
            json!([]),
        );
        inputs.entries = Some(vec![
            json!({"id": "x-epic", "priority": "p1", "status": "in_progress", "type": "epic"}),
            json!({"id": "x-child", "priority": "p1", "status": "in_progress", "parent": "x-epic"}),
            json!({"id": "x-worked", "priority": "p1", "status": "in_progress", "parent": "x-epic"}),
        ]);
        // A worked-feed listing holds the parent with no claim anywhere.
        inputs.worked = ok_read(json!([{"id": "x-worked"}]));
        inputs.holder_activity.insert(
            "h".to_string(),
            crate::truth_probe::TruthProbe {
                state: "working".to_string(),
                harness_title: None,
                reachability: None,
                basis: None,
                last_activity_age_s: Some(30.0),
                last_event_at: None,
                last_message: None,
                observed_model: Value::Null,
                provider_refusal: None,
            },
        );
        let board = build_board(&inputs);
        let queues = board.get("queues").and_then(Value::as_array).unwrap();
        let unheld = queues
            .iter()
            .find(|q| q["name"] == "unheld_progress")
            .unwrap();
        assert_eq!(unheld["rows"].as_array().unwrap().len(), 0, "{unheld}");
    }

    #[test]
    fn unheld_progress_keeps_an_epic_whose_children_all_lack_claims() {
        // The inverse leg is the point of the queue: an epic whose children
        // all died is genuinely stalled and must still reach the king. A done
        // child never holds its parent, whatever its leftover claim says.
        let mut inputs = inputs_with(
            json!([]),
            json!([{"key": "node:x-done", "state": "live", "holder": "h"}]),
            json!([]),
        );
        inputs.entries = Some(vec![
            json!({"id": "x-epic", "priority": "p1", "status": "in_progress", "type": "epic"}),
            json!({"id": "x-done", "priority": "p1", "status": "done", "parent": "x-epic"}),
            json!({"id": "x-dead", "priority": "p1", "status": "in_progress", "parent": "x-epic"}),
        ]);
        let board = build_board(&inputs);
        let queues = board.get("queues").and_then(Value::as_array).unwrap();
        let unheld = queues
            .iter()
            .find(|q| q["name"] == "unheld_progress")
            .unwrap();
        let ids: Vec<&str> = unheld["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["id"].as_str())
            .collect();
        assert!(ids.contains(&"x-epic"), "{unheld}");
    }

    #[test]
    fn unheld_progress_omits_an_epic_whose_child_claim_is_live_but_unmeasured() {
        // The queue is UNHELD progress: a live lock is held even when the
        // holder probe did not answer. Flagging the parent off an
        // unmeasured-but-claimed child would read uncertainty as a dead
        // handoff.
        let mut inputs = inputs_with(
            json!([]),
            json!([{"key": "node:x-child", "state": "live", "holder": "u"}]),
            json!([]),
        );
        inputs.entries = Some(vec![
            json!({"id": "x-epic", "priority": "p1", "status": "in_progress", "type": "epic"}),
            json!({"id": "x-child", "priority": "p1", "status": "in_progress", "parent": "x-epic"}),
        ]);
        let board = build_board(&inputs);
        let queues = board.get("queues").and_then(Value::as_array).unwrap();
        let unheld = queues
            .iter()
            .find(|q| q["name"] == "unheld_progress")
            .unwrap();
        assert_eq!(unheld["rows"].as_array().unwrap().len(), 0, "{unheld}");
    }

    #[test]
    fn unheld_progress_omits_an_epic_held_through_a_nested_sub_epic() {
        // Containers carry no claim and no worked entry of their own, so held
        // state must walk the whole subtree: outer epic -> sub-epic -> worked
        // leaf. Stopping at direct children flags the outer epic forever.
        let mut inputs = inputs_with(
            json!([]),
            json!([{"key": "node:x-leaf", "state": "live", "holder": "h"}]),
            json!([]),
        );
        inputs.entries = Some(vec![
            json!({"id": "x-outer", "priority": "p1", "status": "in_progress", "type": "epic"}),
            json!({"id": "x-sub", "priority": "p1", "status": "in_progress", "type": "epic", "parent": "x-outer"}),
            json!({"id": "x-leaf", "priority": "p1", "status": "in_progress", "parent": "x-sub"}),
        ]);
        inputs.holder_activity.insert(
            "h".to_string(),
            crate::truth_probe::TruthProbe {
                state: "working".to_string(),
                harness_title: None,
                reachability: None,
                basis: None,
                last_activity_age_s: Some(30.0),
                last_event_at: None,
                last_message: None,
                observed_model: Value::Null,
                provider_refusal: None,
            },
        );
        let board = build_board(&inputs);
        let queues = board.get("queues").and_then(Value::as_array).unwrap();
        let unheld = queues
            .iter()
            .find(|q| q["name"] == "unheld_progress")
            .unwrap();
        assert_eq!(unheld["rows"].as_array().unwrap().len(), 0, "{unheld}");
    }

    #[test]
    fn an_expired_lease_under_a_writing_holder_reaches_neither_queue() {
        // AC3-EDGE, the 2026-09-09 measured fault: five nodes sat in
        // unheld_progress AND stale_claim at once because the clock check
        // short-circuited the holder probe. The active verdict is the
        // positive control: it proves the row was read and classified, not
        // dropped by an unrelated guard.
        let node = json!({
            "id": "x-7471",
            "priority": "p1",
            "status": "in_progress",
            "title": "the board probes the holder before it honors the clock",
        });
        let mut inputs = inputs_with(
            json!([]),
            json!([{
                "key": "node:x-7471", "state": "stale",
                "holder": "spawn-handover:target-7471-worker",
            }]),
            json!([]),
        );
        inputs.entries = Some(vec![node.clone()]);
        inputs.holder_activity.insert(
            "target-7471-worker".to_string(),
            crate::truth_probe::TruthProbe {
                state: "working".to_string(),
                provider_refusal: None,
                harness_title: None,
                reachability: None,
                basis: None,
                last_activity_age_s: Some(30.0),
                last_event_at: None,
                last_message: None,
                observed_model: Value::Null,
            },
        );
        let board = build_board(&inputs);
        let queues = board.get("queues").and_then(Value::as_array).unwrap();
        let unheld = queues
            .iter()
            .find(|q| q["name"] == "unheld_progress")
            .unwrap();
        let unheld_ids: Vec<&str> = unheld["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["id"].as_str())
            .collect();
        assert_eq!(unheld_ids, Vec::<&str>::new(), "{unheld}");
        let stale = queues.iter().find(|q| q["name"] == "stale_claim").unwrap();
        let stale_keys: Vec<&str> = stale["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["key"].as_str())
            .collect();
        assert_eq!(stale_keys, Vec::<&str>::new(), "{stale}");
        // Positive control on the same inputs: the node classifies active.
        let claim_by_node: HashMap<String, Value> = [(
            "x-7471".to_string(),
            json!({
                "key": "node:x-7471", "state": "stale",
                "holder": "spawn-handover:target-7471-worker",
            }),
        )]
        .into_iter()
        .collect();
        let (state, _) = node_driver(&node, &claim_by_node, &inputs.holder_activity, None, None);
        assert_eq!(state, "active");
    }

    #[test]
    fn a_failed_truth_batch_reads_the_five_queues_unreadable() {
        // AC5-ERR: the batch's Err must arrive as an unreadable queue, never
        // as rows about workers nobody measured. One Err, five queues blind,
        // exit code 1: the king is told the board cannot see, which is the
        // honest answer.
        let node = json!({"id": "x-blind", "priority": "p0", "status": "in_progress"});
        let claims = json!([{"key": "node:x-blind", "state": "live", "holder": "h"}]);
        let mut inputs = inputs_with(json!([]), claims, json!([node]));
        inputs.holder_activity_error =
            Some("truth probe: batch of 19 handles timed out".to_string());
        let board = build_board(&inputs);
        assert_eq!(board["exit_code"], 1);
        let queues = board["queues"].as_array().unwrap();
        for name in [
            "stalled_holder",
            "stale_claim",
            "unheld_progress",
            "undriven_pr",
            "unplanned",
        ] {
            let q = queues.iter().find(|q| q["name"] == name).expect(name);
            assert_eq!(q["status"], "unreadable", "{name}");
            assert_eq!(q["rows"].as_array().unwrap().len(), 0, "{name}");
        }
    }

    #[test]
    fn an_unreadable_claims_source_reads_the_claims_queues_unreadable() {
        // AC5-ERR (x-636f): the claims source's Err must arrive as unreadable
        // queues, never as an empty-fleet success that would re-dispatch held
        // work. This pins the existing `!inputs.claims.is_ok()` gates, which
        // had no test on the claims leg.
        let node = json!({"id": "x-blind", "priority": "p0", "status": "in_progress"});
        let mut inputs = inputs_with(json!([]), json!([]), json!([node]));
        inputs.claims = SourceRead::err("claims unreadable: x");
        let board = build_board(&inputs);
        let queues = board["queues"].as_array().unwrap();
        for name in ["unheld_progress", "undriven_pr", "stale_claim"] {
            let q = queues.iter().find(|q| q["name"] == name).expect(name);
            assert_eq!(q["status"], "unreadable", "{name}");
            assert_eq!(q["rows"].as_array().unwrap().len(), 0, "{name}");
        }
    }

    #[test]
    fn a_partially_answered_batch_names_its_unmeasured_holders() {
        // AC6-EDGE: the batch answered but skipped one holder. Its node lands
        // in no queue row, and the payload warnings name the hole so a
        // partial answer cannot pass for a full one.
        let node = json!({"id": "x-half", "priority": "p0", "status": "in_progress"});
        let claims = json!([{"key": "node:x-half", "state": "live", "holder": "h"}]);
        let mut inputs = inputs_with(json!([]), claims, json!([node]));
        inputs.entries = Some(vec![node]);
        let board = build_board(&inputs);
        let queues = board["queues"].as_array().unwrap();
        for name in [
            "stalled_holder",
            "stale_claim",
            "unheld_progress",
            "undriven_pr",
            "unplanned",
        ] {
            let q = queues.iter().find(|q| q["name"] == name).expect(name);
            assert_eq!(
                q["rows"].as_array().unwrap().len(),
                0,
                "{name} must not row an unmeasured holder"
            );
        }
        let warnings = board["warnings"].as_array().unwrap();
        assert!(
            warnings.iter().any(|w| w
                .as_str()
                .map(|s| s.contains("unmeasured: h"))
                .unwrap_or(false)),
            "{warnings:?}"
        );
    }

    #[test]
    fn a_working_handover_holder_keeps_its_node_out_of_unplanned_and_stale() {
        // AC7-HP: the ready feed cannot see a stale launch-window lease
        // (include_stale=false excludes it, worked ids too), so the old
        // `!dead` filter left a node under an advancing worker listed as
        // unplanned forever. The driver join drops it; a dead claim still
        // belongs to stale_claim alone.
        let mut inputs = inputs_with(
            json!([{"id": "x-hold", "priority": "p1", "title": "underway"}]),
            json!([{
                "key": "node:x-hold", "state": "stale",
                "holder": "spawn-handover:t-w",
            }]),
            json!([]),
        );
        inputs.holder_activity.insert(
            "t-w".to_string(),
            crate::truth_probe::TruthProbe {
                state: "working".to_string(),
                provider_refusal: None,
                harness_title: None,
                reachability: None,
                basis: None,
                last_activity_age_s: Some(30.0),
                last_event_at: None,
                last_message: None,
                observed_model: Value::Null,
            },
        );
        let board = build_board(&inputs);
        let queues = board["queues"].as_array().unwrap();
        let unplanned = queues.iter().find(|q| q["name"] == "unplanned").unwrap();
        assert_eq!(
            unplanned["rows"].as_array().unwrap().len(),
            0,
            "{unplanned}"
        );
        let stale = queues.iter().find(|q| q["name"] == "stale_claim").unwrap();
        assert_eq!(stale["rows"].as_array().unwrap().len(), 0, "{stale}");
    }

    #[test]
    fn a_live_handover_lock_on_an_in_progress_node_reads_through_the_scan() {
        // AC9-HP, read through scan_claims_dir: this is the test a
        // lease-keyed skip in the scan cannot survive. The skip removed the
        // row pre-classification, so an in_progress node in its launch window
        // read driver-none and landed in unheld_progress - the exact silence
        // the classify tests cannot see past.
        let dir = std::env::temp_dir().join(format!("kb-board-handover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let now = crate::claims::now_ms();
        let yaml = format!(
            "schema_version: 1\nkey: \"node:x-lease\"\nholder: \"spawn-handover:t-w\"\nacquired_at: {now}\npid: 1\nhost: test-host\nexpires_at: {}\nreason: \"spawn handover window for node:x-lease\"\n",
            now + 900_000
        );
        std::fs::write(dir.join("node%3Ax-lease.lock"), yaml).expect("write lock");
        let rows = claims::read_claims_in(std::slice::from_ref(&dir)).rows();
        assert_eq!(rows.len(), 1, "the live lock is one row: {rows:?}");
        let node = json!({"id": "x-lease", "priority": "p0", "status": "in_progress"});
        let mut inputs = inputs_with(json!([]), json!(rows), json!([node.clone()]));
        inputs.entries = Some(vec![node]);
        let working_probe = |age_s: f64| crate::truth_probe::TruthProbe {
            state: "working".to_string(),
            provider_refusal: None,
            harness_title: None,
            reachability: None,
            basis: None,
            last_activity_age_s: Some(age_s),
            last_event_at: None,
            last_message: None,
            observed_model: Value::Null,
        };
        // A working worker 30s into its run: the launch window holds a live
        // driver, so the node reads in NO queue row.
        inputs
            .holder_activity
            .insert("t-w".to_string(), working_probe(30.0));
        let board = build_board(&inputs);
        let queues = board["queues"].as_array().unwrap();
        let stalled = queues
            .iter()
            .find(|q| q["name"] == "stalled_holder")
            .unwrap();
        assert_eq!(stalled["rows"].as_array().unwrap().len(), 0, "{stalled}");
        let unheld = queues
            .iter()
            .find(|q| q["name"] == "unheld_progress")
            .unwrap();
        assert_eq!(unheld["rows"].as_array().unwrap().len(), 0, "{unheld}");
        // Positive control on the same lock: a working worker PAST the stall
        // clock lists in stalled_holder. The row reached the queue, so the
        // zero above is a verdict about the worker, not a lost row.
        inputs.holder_activity.insert(
            "t-w".to_string(),
            working_probe(crate::king_board::classify::STALLED_AFTER_S + 1.0),
        );
        let board = build_board(&inputs);
        let queues = board["queues"].as_array().unwrap();
        let stalled = queues
            .iter()
            .find(|q| q["name"] == "stalled_holder")
            .unwrap();
        let ids: Vec<&str> = stalled["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["id"].as_str())
            .collect();
        assert_eq!(ids, vec!["x-lease"], "{stalled}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mergeable_pr_is_scoped_by_the_binding_node() {
        // On a scoped board a PR whose node cannot be resolved fails
        // CLOSED - unknown attribution is not every crown's work. PR 1494 is
        // attributed outside the crown (out_of_scope); PR 99 is bound by no
        // node at all, so it lands in no queue and the warning names the drop.
        let mut inputs = inputs_with(json!([]), json!([]), json!([]));
        inputs.prs = ok_read(json!([
            {"number": 1494, "title": "foreign"},
            {"number": 99, "title": "unbound"},
        ]));
        inputs.pr_nodes = ok_read(json!([
            {"id": "x-out", "priority": "p1", "pr_number": 1494},
        ]));
        inputs.scope_ids = Some(["x-in"].into_iter().map(str::to_string).collect());
        inputs.crown_scope = Some("x-crown".to_string());
        let board = build_board(&inputs);
        let queues = board.get("queues").and_then(Value::as_array).unwrap();
        let mergeable = queues.iter().find(|q| q["name"] == "mergeable_pr").unwrap();
        let numbers: Vec<i64> = mergeable["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["number"].as_i64())
            .collect();
        assert_eq!(numbers, Vec::<i64>::new(), "{mergeable}");
        let out_of_scope = queues.iter().find(|q| q["name"] == "out_of_scope").unwrap();
        let mut ids: Vec<&str> = out_of_scope["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["id"].as_str())
            .collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids, vec!["x-out"], "{out_of_scope}");
        let warnings = board["warnings"].as_array().unwrap();
        assert!(
            warnings
                .iter()
                .any(|w| w.as_str().unwrap_or("").contains("unattributed")
                    && w.as_str().unwrap_or("").contains("mergeable_pr")),
            "{warnings:?}"
        );
    }

    #[test]
    fn unscoped_board_still_shows_a_pr_no_node_binds() {
        // The fail-closed verdict is scoped-board only. The operator
        // board (no scope_ids) keeps showing every PR, unattributable or not.
        let mut inputs = inputs_with(json!([]), json!([]), json!([]));
        inputs.prs = ok_read(json!([
            {"number": 99, "title": "unbound"},
        ]));
        let board = build_board(&inputs);
        let queues = board.get("queues").and_then(Value::as_array).unwrap();
        let mergeable = queues.iter().find(|q| q["name"] == "mergeable_pr").unwrap();
        let numbers: Vec<i64> = mergeable["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["number"].as_i64())
            .collect();
        assert_eq!(numbers, vec![99], "{mergeable}");
        assert!(board["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .all(|w| !w.as_str().unwrap_or("").contains("unattributed")));
    }

    #[test]
    fn scoped_board_keeps_report_only_signals_whose_node_is_none_by_design() {
        // Companion pin: needs.rs mints node: None for mail_escalation,
        // carveout_stale, stale_claims and worker_refused on purpose - they
        // describe a worker, not a node. A report-only queue bypasses the
        // crown filter, so a scoped board still sees the distress signal and
        // the drop is not counted unattributed.
        let mut inputs = inputs_with(json!([]), json!([]), json!([]));
        inputs.needs = ok_read(json!([
            {"kind": "worker_refused", "name": "w-1", "node": null},
            {"kind": "mail_escalation", "name": "m-1", "node": null},
        ]));
        inputs.scope_ids = Some(["x-in"].into_iter().map(str::to_string).collect());
        let board = build_board(&inputs);
        let queues = board.get("queues").and_then(Value::as_array).unwrap();
        let unreachable = queues
            .iter()
            .find(|q| q["name"] == "unreachable_worker")
            .unwrap();
        assert_eq!(
            unreachable["rows"].as_array().unwrap().len(),
            2,
            "{unreachable}"
        );
        assert!(board["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .all(|w| !w.as_str().unwrap_or("").contains("unattributed")));
    }

    #[test]
    fn ac1_blocked_child_names_the_node_session_reason_and_age() {
        // AC1-HP: an in-scope unanswered blocked row names node, session,
        // reason, and age when the board runs from the crowned session.
        let mut inputs = inputs_with(json!([]), json!([]), json!([]));
        inputs.blocked_child = ok_read(json!([{
            "id": "x-eb79",
            "session": "cx-run-1",
            "reason": "worktree-init-blocked",
            "evidence": "Operation not permitted",
            "age_minutes": 45,
        }]));
        inputs.scope_ids = Some(["x-eb79"].into_iter().map(str::to_string).collect());
        let board = build_board(&inputs);
        let queues = board.get("queues").and_then(Value::as_array).unwrap();
        let blocked = queues
            .iter()
            .find(|q| q["name"] == "blocked_child")
            .unwrap();
        let rows = blocked["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{blocked}");
        assert_eq!(rows[0]["id"], "x-eb79");
        assert_eq!(rows[0]["session"], "cx-run-1");
        assert_eq!(rows[0]["reason"], "worktree-init-blocked");
        assert_eq!(rows[0]["age_minutes"], 45);
    }

    #[test]
    fn ac2_blocked_child_row_outside_scope_is_absent_and_counted_out_of_scope() {
        // AC2-EDGE: the converse of AC1, exactly like every sibling queue.
        let mut inputs = inputs_with(json!([]), json!([]), json!([]));
        inputs.blocked_child = ok_read(json!([{
            "id": "x-foreign",
            "session": "cx-run-2",
            "reason": "missing dependency",
            "evidence": null,
            "age_minutes": 60,
        }]));
        inputs.scope_ids = Some(["x-in-scope"].into_iter().map(str::to_string).collect());
        inputs.crown_scope = Some("x-crown".to_string());
        let board = build_board(&inputs);
        let queues = board.get("queues").and_then(Value::as_array).unwrap();
        let blocked = queues
            .iter()
            .find(|q| q["name"] == "blocked_child")
            .unwrap();
        assert_eq!(blocked["rows"].as_array().unwrap().len(), 0, "{blocked}");
        let out_of_scope = queues.iter().find(|q| q["name"] == "out_of_scope").unwrap();
        let ids: Vec<&str> = out_of_scope["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["id"].as_str())
            .collect();
        assert_eq!(ids, vec!["x-foreign"], "{out_of_scope}");
    }

    /// Serializes the tests that point process-global HOME at a temp dir:
    /// every other test in this binary reads HOME, so a concurrent reader can
    /// catch it mid-flip (the same ENV_LOCK shape client_tests uses).
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn the_board_answers_inside_a_tight_budget_with_every_queue_present() {
        // An isolated HOME + cwd: no graph, no claims, no lane - the degraded
        // machine. The board must still answer with all thirteen queues
        // (plus nothing else), the unreadable actionable ones counted, and
        // exit 1.
        // A declared hermetic root: blocked_child's collection resolves
        // ~/.fno/agents via AgentsHome, which panics under test with no
        // declared root (paths.rs) - this test already pins HOME, so it also
        // pins FNO_AGENTS_HOME under the same tempdir to declare one.
        let _guard = HOME_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", dir.path());
        std::env::set_var("FNO_AGENTS_HOME", dir.path().join(".fno").join("agents"));
        let payload = read_board(&BoardOpts {
            budget_ms: 20_000,
            ..Default::default()
        });
        std::env::remove_var("FNO_AGENTS_HOME");
        let queues = payload.get("queues").and_then(Value::as_array).unwrap();
        assert_eq!(queues.len(), 13, "{payload}");
        assert_eq!(payload["exit_code"], 1, "{payload}");
        assert!(payload["unreadable"].as_i64().unwrap() > 0);
        let names: Vec<&str> = queues
            .iter()
            .filter_map(|q| q.get("name").and_then(Value::as_str))
            .collect();
        assert_eq!(
            names,
            vec![
                "operator_lane",
                "undispatched",
                "unplanned",
                "stalled_holder",
                "unheld_progress",
                "blocked_child",
                "undriven_pr",
                "mergeable_pr",
                "stale_claim",
                "operator_question",
                "carveout_pending",
                "capture_pending",
                "unreachable_worker",
            ]
        );
        // The done-probe's contract: every named source carries its verdict.
        let sources = payload.get("sources").and_then(Value::as_object).unwrap();
        assert!(!sources.is_empty());
        for (_name, s) in sources {
            assert!(s.get("ok").is_some() && s.get("truncated").is_some(), "{s}");
        }
    }

    #[test]
    fn the_scope_error_queue_is_actionable_and_loud() {
        let _guard = HOME_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("king.md");
        std::fs::write(&state, "---\nscope: not-a-real-thing\n---\n").unwrap();
        std::env::set_var("HOME", dir.path());
        let payload = read_board(&BoardOpts {
            budget_ms: 20_000,
            state_path: Some(state),
            ..Default::default()
        });
        let queues = payload.get("queues").and_then(Value::as_array).unwrap();
        assert_eq!(queues.len(), 1);
        assert_eq!(queues[0]["name"], "scope");
        assert_eq!(queues[0]["status"], "unreadable");
        assert_eq!(queues[0]["actionable"], true);
        assert_eq!(payload["exit_code"], 1);
    }

    #[test]
    fn a_manifest_without_a_scope_is_a_scope_error() {
        let _guard = HOME_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("king.md");
        std::fs::write(&state, "---\nfno_id: k1\n---\n").unwrap();
        std::env::set_var("HOME", dir.path());
        let payload = read_board(&BoardOpts {
            budget_ms: 20_000,
            state_path: Some(state),
            ..Default::default()
        });
        let queues = payload.get("queues").and_then(Value::as_array).unwrap();
        assert_eq!(queues[0]["error"], "king manifest has no scope");
    }
}
